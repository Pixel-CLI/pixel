# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Stdin: Claude settings JSON. Stdout: JSON with a pixel hook set added.
Default (no arg or "quiet"): session-start + post-compaction only — no
prompt-submit packet, no relays. Pass "full" for the complete lifecycle set."""
import json, sys
from pathlib import Path

exe = sys.argv[1] if len(sys.argv) > 1 else str(Path.home() / ".local/bin/pixel")
mode = sys.argv[2] if len(sys.argv) > 2 else "quiet"

def group(matcher, verb):
    return {"matcher": matcher, "hooks": [{"type": "command",
            "command": f"'{exe}' run-hook {verb} --provider claude", "timeout": 10}]}

s = json.load(sys.stdin)
h = s.setdefault("hooks", {})
h.setdefault("SessionStart", []).extend([group("", "session-start"), group("compact", "post-compaction")])
if mode == "full":
    h.setdefault("PostToolUse", []).extend([group("Edit", "post-tool-use"), group("Bash", "metrics")])
    h.setdefault("UserPromptSubmit", []).extend([group("", "prompt-submit")])
print(json.dumps(s))
