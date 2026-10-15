#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT
"""Split one Claude Code session transcript into turns.

A turn starts at a human prompt and holds what the UserPromptSubmit hooks added (the
brief), the tool calls the agent made, and its last text. Used by `arena.sh run-set
--keep-context`, where one session answers every question of a set.

    session-digest.py FILE                 all turns, JSON
    session-digest.py FILE --turn N        turn N (1-based), JSON
    session-digest.py FILE --hide-prompt   the prompt is replaced by its sha256 prefix
    session-digest.py A.jsonl --md B.jsonl a Markdown report of the two arms, turn by turn
"""
import hashlib
import json
import sys


def blocks(message):
    content = (message or {}).get("content")
    return content if isinstance(content, list) else []


def prompt_text(entry):
    content = (entry.get("message") or {}).get("content")
    if isinstance(content, str):
        return content
    parts = blocks(entry.get("message"))
    if not parts or any(b.get("type") == "tool_result" for b in parts):
        return None
    return "".join(b.get("text", "") for b in parts if b.get("type") == "text") or None


def call_of(block):
    args = block.get("input") or {}
    for key in ("command", "file_path", "pattern", "query", "path"):
        if args.get(key):
            return str(args[key])
    return json.dumps(args, sort_keys=True)


def turns(path, hide_prompt=False):
    out, cur = [], None
    with open(path, encoding="utf-8", errors="replace") as handle:
        for line in handle:
            try:
                entry = json.loads(line)
            except ValueError:
                continue
            kind = entry.get("type")
            if kind == "user" and not entry.get("isMeta"):
                text = prompt_text(entry)
                if text:
                    digest = hashlib.sha256(text.encode()).hexdigest()[:16]
                    cur = {"n": len(out) + 1, "prompt": f"sha256:{digest}" if hide_prompt else text,
                           "timestamp": entry.get("timestamp"), "briefs": [], "tool_calls": [], "answer": None}
                    out.append(cur)
            elif cur is None:
                continue
            elif kind == "attachment":
                att = entry.get("attachment") or {}
                if att.get("type") == "hook_additional_context" and att.get("hookEvent") == "UserPromptSubmit":
                    body = att.get("content")
                    body = "\n".join(body) if isinstance(body, list) else str(body or "")
                    if body.strip():
                        cur["briefs"].append(body)
            elif kind == "assistant":
                for block in blocks(entry.get("message")):
                    if block.get("type") == "tool_use":
                        cur["tool_calls"].append({"name": block.get("name"), "call": call_of(block)})
                    elif block.get("type") == "text" and block.get("text", "").strip():
                        cur["answer"] = block["text"]
    return out


def markdown(pixel, nopixel):
    lines = ["# Session report: every brief and tool call, turn by turn", ""]
    for i in range(max(len(pixel), len(nopixel))):
        p = pixel[i] if i < len(pixel) else None
        n = nopixel[i] if i < len(nopixel) else None
        head = (p or n)["prompt"]
        lines += [f"## Turn {i + 1}", "", f"> {head}", ""]
        if p and n and p["prompt"] != n["prompt"]:
            lines += [f"**The two arms answered different prompts in this turn.** Without Pixel: {n['prompt']}", ""]
        for label, t in (("with Pixel", p), ("without Pixel", n)):
            if t is None:
                continue
            lines += [f"### {label}: {len(t['tool_calls'])} tool call(s), brief {sum(len(b.encode()) for b in t['briefs'])} bytes", ""]
            for b in t["briefs"]:
                lines += ["<details><summary>brief</summary>", "", "```", b, "```", "</details>", ""]
            for c in t["tool_calls"]:
                lines.append(f"- `{c['name']}` {c['call'][:200]}")
            lines += ["", "answer:", "", (t["answer"] or "(none)"), ""]
    return "\n".join(lines)


def main(argv):
    if len(argv) < 2:
        sys.exit(__doc__)
    path, hide = argv[1], "--hide-prompt" in argv
    if "--md" in argv:
        other = argv[argv.index("--md") + 1]
        print(markdown(turns(path, hide), turns(other, hide)))
        return
    result = turns(path, hide)
    if "--turn" in argv:
        n = int(argv[argv.index("--turn") + 1])
        result = result[n - 1] if 0 < n <= len(result) else None
    print(json.dumps(result, indent=1))


if __name__ == "__main__":
    main(sys.argv)
