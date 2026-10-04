# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Filter the pixel entries of a hooks document for one arm.

Stdin: a Claude `settings.json` / `settings.local.json` or a Codex
`hooks.json` (both are {"hooks": {Event: [{matcher?, hooks: [{command}]}]}}).
Stdout: the same document with its pixel hooks kept according to <mode>:

  none   every pixel hook removed (the baseline arm)
  quiet  only the session-start doctrine: `run-hook session-start` and
         `run-hook post-compaction` (no prompt-submit task packet, no
         metrics relay, no guard, no task-event hooks)
  full   every pixel hook kept as installed (the install default)

Non-pixel hooks always survive untouched, so the variable between arms is
the pixel hook set alone. A hook is "pixel" when its command mentions
pixel at all, the rule strip_pixel_hooks.py has always applied, so `none`
and the legacy baseline strip the same entries.
"""
import json
import re
import sys

QUIET = re.compile(r"run-hook\s+(?:session-start|post-compaction)\b")


def keep(command: str, mode: str) -> bool:
    if "pixel" not in command:
        return True
    if mode == "full":
        return True
    if mode == "quiet":
        return bool(QUIET.search(command))
    return False


def filter_doc(doc: dict, mode: str) -> dict:
    if mode not in ("none", "quiet", "full"):
        raise SystemExit(f"unknown mode {mode!r}")
    hooks = doc.get("hooks")
    if not isinstance(hooks, dict):
        return doc
    out = {}
    for event, groups in hooks.items():
        kept_groups = []
        for group in groups:
            kept = [h for h in group.get("hooks", []) if keep(h.get("command", ""), mode)]
            if kept:
                kept_groups.append({**group, "hooks": kept})
        if kept_groups:
            out[event] = kept_groups
    doc = dict(doc)
    if out:
        doc["hooks"] = out
    else:
        doc.pop("hooks", None)
    return doc


def main() -> None:
    mode = sys.argv[1] if len(sys.argv) > 1 else "none"
    print(json.dumps(filter_doc(json.load(sys.stdin), mode), indent=2))


if __name__ == "__main__":
    main()
