#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT
"""Fail-closed audit and receipt wrapper for disposable arena Codex hooks."""

from __future__ import annotations

import hashlib
import json
import os
import shlex
import subprocess
import sys
from pathlib import Path

PIXEL = "/usr/local/bin/pixel"
EVENTS = (
    "session-start", "prompt-submit", "pre-tool-use", "post-tool-use", "stop",
    "session-end", "subagent-start", "subagent-stop", "interrupt",
)
ALLOWED = {
    (PIXEL, "run-hook", "prompt-submit", "--provider", "codex"),
    (PIXEL, "run-hook", "metrics", "--provider", "codex"),
    *((PIXEL, "run-hook", "task-event", "--provider", "codex", "--event", event)
      for event in EVENTS),
}
SOURCE_NAMES = {"hooks.json", "config.toml", "requirements.toml", "plugin.json", "installed_plugins.json"}


def commands(value: object):
    if isinstance(value, dict):
        if "type" in value:
            if (
                value.get("type") != "command"
                or not isinstance(value.get("command"), str)
                or set(value) - {"type", "command", "timeout"}
            ):
                raise RuntimeError("non-command or unsupported Codex hook object is configured")
            yield value["command"]
            return
        if "hooks" in value:
            if set(value) - {"hooks", "matcher"} or not isinstance(value["hooks"], list):
                raise RuntimeError("unrecognized Codex hook container shape")
            if "matcher" in value and not isinstance(value["matcher"], str):
                raise RuntimeError("unrecognized Codex hook matcher shape")
            yield from commands(value["hooks"])
            return
        if "command" in value or "prompt" in value or "agent" in value:
            raise RuntimeError("Codex hook command has no supported command type")
        for key, child in value.items():
            if key != "enabled":
                yield from commands(child)
    elif isinstance(value, list):
        for child in value:
            yield from commands(child)
    elif value not in (None, False, ""):
        raise RuntimeError("unrecognized nonempty Codex hook-tree leaf")


def source_files(roots: list[Path]) -> list[Path]:
    found: set[Path] = set()
    for root in roots:
        if root.is_file():
            if root.name in SOURCE_NAMES:
                found.add(root)
            continue
        if not root.is_dir():
            continue
        for directory, dirs, files in os.walk(root):
            dirs[:] = [name for name in dirs if name not in {"sessions", "logs", "state"}]
            found.update(Path(directory) / name for name in files if name in SOURCE_NAMES)
    return sorted(found)


def parse(path: Path):
    text = path.read_text()
    if path.suffix == ".json":
        return json.loads(text)
    import tomllib
    return tomllib.loads(text)


def replace_prompt(value: object, old: str, new: str) -> int:
    count = 0
    if isinstance(value, dict):
        if value.get("command") == old:
            value["command"] = new
            count += 1
        for key, child in value.items():
            if key != "command":
                count += replace_prompt(child, old, new)
    elif isinstance(value, list):
        count = sum(replace_prompt(child, old, new) for child in value)
    return count


def audit(
    arm: str, codex_home: Path, repo: Path, receipt: Path, rep: str,
    system_codex: Path = Path("/etc/codex"),
) -> None:
    if not receipt.is_absolute():
        raise RuntimeError("audit receipt path must be absolute")
    files = source_files([codex_home, repo / ".codex", system_codex])
    found: list[tuple[Path, str, tuple[str, ...]]] = []
    for path in files:
        value = parse(path)
        if path.name == "plugin.json" and isinstance(value, dict) and value.get("hooks"):
            raise RuntimeError(f"plugin hook declarations are present in {path}")
        if path.name == "installed_plugins.json" and (
            value.get("plugins") if isinstance(value, dict) else value
        ):
            raise RuntimeError(f"installed Codex plugins are present in {path}")
        if path.name in {"config.toml", "requirements.toml"} and isinstance(value, dict) and value.get("plugins"):
            raise RuntimeError(f"Codex plugin registry is configured in {path}")
        tree = value.get("hooks") if isinstance(value, dict) else None
        for command in commands(tree):
            try:
                argv = tuple(shlex.split(command))
            except ValueError as exc:
                raise RuntimeError(f"unparseable hook command in {path}") from exc
            if argv not in ALLOWED:
                raise RuntimeError(f"foreign or unrecognized hook command in {path}")
            found.append((path, command, argv))

    if arm == "raw" and found:
        raise RuntimeError("raw arm must have no hook commands")
    if arm == "pixel":
        prompt = [item for item in found if item[2] == (PIXEL, "run-hook", "prompt-submit", "--provider", "codex")]
        if len(found) != len(ALLOWED) or {item[2] for item in found} != ALLOWED:
            raise RuntimeError("Pixel hook set does not match the audited 11-command allowlist")
        if any(item[0] != codex_home / "hooks.json" for item in found):
            raise RuntimeError("Pixel hooks must all be in the disposable user-level hooks.json")
        if len(prompt) != 1 or prompt[0][0] != codex_home / "hooks.json":
            raise RuntimeError("expected one user-level Pixel prompt-submit hook")
        path, old, _ = prompt[0]
        receipt_path = Path(f"/out/pixel-hook-{rep}.jsonl")
        if not receipt_path.is_absolute():
            raise RuntimeError("hook receipt path must be absolute")
        wrapper = "python3 /usr/local/lib/arena-hook-audit.py --receipt " + shlex.quote(str(receipt_path))
        wrapper += " -- " + old
        value = json.loads(path.read_text())
        changed = replace_prompt(value.get("hooks", {}), old, wrapper)
        if changed != 1:
            raise RuntimeError(f"expected to wrap one prompt-submit command, found {changed}")
        path.write_text(json.dumps(value, indent=2) + "\n")

    receipt.parent.mkdir(parents=True, exist_ok=True)
    receipt.write_text(json.dumps({
        "arm": arm,
        "hook_sources": [str(path) for path in files],
        "commands": [list(item[2]) for item in found],
        "foreign_commands": 0,
        "trust_bypass_safe": True,
        "prompt_hook_wrapped": arm == "pixel",
    }, indent=2) + "\n")


def run_hook(receipt: Path, argv: list[str]) -> int:
    if tuple(argv) not in ALLOWED or argv[2] != "prompt-submit":
        raise RuntimeError("refusing to execute a command outside the exact Pixel prompt-hook allowlist")
    if not receipt.is_absolute():
        raise RuntimeError("hook receipt path must be absolute")
    proc = subprocess.run(argv, input=sys.stdin.buffer.read(), stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    sys.stderr.buffer.write(proc.stderr)
    sys.stderr.buffer.flush()
    event_name = None
    context = None
    valid = False
    if proc.returncode == 0 and not proc.stdout.strip():
        # An empty successful response is Codex's valid no-op/abstention shape.
        valid = True
    elif proc.returncode == 0:
        try:
            response = json.loads(proc.stdout)
            if isinstance(response, dict):
                specific = response.get("hookSpecificOutput")
                if specific is None:
                    valid = True
                elif isinstance(specific, dict):
                    event_name = specific.get("hookEventName")
                    context = specific.get("additionalContext")
                    valid = (
                        event_name == "UserPromptSubmit"
                        and ("additionalContext" not in specific or isinstance(context, str))
                    )
        except (ValueError, AttributeError):
            pass
    record = {
        "returncode": proc.returncode,
        "stderr": proc.stderr.decode(errors="replace"),
        "response_valid": valid,
        "forwarded_to_codex": valid and isinstance(context, str) and bool(context.strip()),
        "emitted_context": isinstance(context, str) and bool(context.strip()),
        "hook_event_name": event_name,
        "additional_context": context if isinstance(context, str) else None,
        "additional_context_bytes": len(context.encode()) if isinstance(context, str) else 0,
        "additional_context_sha256": hashlib.sha256(context.encode()).hexdigest() if isinstance(context, str) else None,
    }
    receipt.parent.mkdir(parents=True, exist_ok=True)
    with receipt.open("a") as stream:
        stream.write(json.dumps(record) + "\n")
    sys.stdout.buffer.write(proc.stdout)
    sys.stdout.buffer.flush()
    return proc.returncode if valid else (proc.returncode or 1)


def main() -> int:
    args = sys.argv[1:]
    if args and args[0] == "audit":
        import argparse
        parser = argparse.ArgumentParser()
        parser.add_argument("audit", choices=["audit"])
        parser.add_argument("--arm", required=True, choices=["raw", "pixel"])
        parser.add_argument("--codex-home", type=Path, required=True)
        parser.add_argument("--repo", type=Path, required=True)
        parser.add_argument("--receipt", type=Path, required=True)
        parser.add_argument("--rep", default="1")
        parser.add_argument("--system-codex", type=Path, default=Path("/etc/codex"))
        options = parser.parse_args(args)
        audit(options.arm, options.codex_home, options.repo, options.receipt, options.rep, options.system_codex)
        return 0
    if len(args) >= 3 and args[0] == "--receipt" and "--" in args:
        split = args.index("--")
        return run_hook(Path(args[1]), args[split + 1:])
    raise SystemExit("usage: hook_audit.py audit --arm raw|pixel --codex-home PATH --repo PATH --receipt PATH [--rep N]")


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, RuntimeError, ValueError, json.JSONDecodeError) as exc:
        print(f"arena hook audit failed: {exc}", file=sys.stderr)
        raise SystemExit(1)
