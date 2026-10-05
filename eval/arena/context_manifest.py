#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Capture static Codex instructions that can affect an arena answer."""
import argparse
import hashlib
import json
from pathlib import Path
import tomllib


def sha256(text):
    return hashlib.sha256(text.encode("utf-8")).hexdigest()


def semantic_text(text):
    return text.replace("\r\n", "\n").replace("\r", "\n").strip()


def collect_manifest(repo, codex_home):
    repo = Path(repo)
    codex_home = Path(codex_home)
    config_path = codex_home / "config.toml"
    developer_instructions = None
    if config_path.is_file():
        config = tomllib.loads(config_path.read_text())
        raw_instructions = config.get("developer_instructions")
        if raw_instructions is not None and not isinstance(raw_instructions, str):
            raise ValueError("Codex developer_instructions must be a string")
        developer_instructions = semantic_text(raw_instructions or "")
    else:
        developer_instructions = ""

    agents = {}
    global_agents = codex_home / "AGENTS.md"
    if global_agents.is_file():
        agents["$CODEX_HOME/AGENTS.md"] = sha256(global_agents.read_text())
    excluded = {".git", "node_modules", "target", ".next", "dist", "build"}
    for path in sorted(repo.rglob("AGENTS.md")):
        if excluded.intersection(path.relative_to(repo).parts):
            continue
        agents[path.relative_to(repo).as_posix()] = sha256(path.read_text())

    return {
        "developer_instructions": {
            "nonempty": bool(developer_instructions),
            "sha256": sha256(developer_instructions) if developer_instructions else None,
            "characters": len(developer_instructions),
        },
        "agents_files": agents,
        "sources": ["Codex developer_instructions", "global/project AGENTS.md"],
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", required=True)
    parser.add_argument("--codex-home", required=True)
    args = parser.parse_args()
    print(json.dumps(collect_manifest(args.repo, args.codex_home), sort_keys=True))


if __name__ == "__main__":
    main()
