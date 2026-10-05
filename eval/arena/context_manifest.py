#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Capture static Codex instructions that can affect an arena answer."""
import argparse
import hashlib
import json
from pathlib import Path
import re
import tomllib


def sha256(text):
    return hashlib.sha256(text.encode("utf-8")).hexdigest()


def semantic_text(text):
    return text.replace("\r\n", "\n").replace("\r", "\n").strip()


def collect_skills(repo, codex_home):
    roots = {}
    repo = Path(repo)
    for ancestor in reversed((repo, *repo.parents)):
        roots[f"repo:{ancestor}/.agents/skills"] = ancestor / ".agents" / "skills"
    roots["$HOME/.agents/skills"] = Path(codex_home).parent / ".agents" / "skills"
    roots["$CODEX_HOME/skills"] = Path(codex_home) / "skills"
    roots["$CODEX_HOME/.agents/skills"] = Path(codex_home) / ".agents" / "skills"
    roots["/etc/codex/skills"] = Path("/etc/codex/skills")

    skills = {}
    for root_label, root in roots.items():
        if not root.is_dir():
            continue
        for skill_file in sorted(root.glob("*/SKILL.md")):
            if not skill_file.is_file():
                continue
            key = f"{root_label}/{skill_file.parent.name}"
            files = sorted(path for path in skill_file.parent.rglob("*") if path.is_file())
            content_hash = hashlib.sha256()
            pixel_mentions = 0
            for path in files:
                content_hash.update(path.relative_to(skill_file.parent).as_posix().encode())
                content_hash.update(b"\0")
                contents = path.read_bytes()
                content_hash.update(contents)
                content_hash.update(b"\0")
                try:
                    pixel_mentions += sum(
                        "pixel" in line.casefold()
                        for line in contents.decode("utf-8").splitlines()
                    )
                except UnicodeDecodeError:
                    pass
            policy_file = skill_file.parent / "agents" / "openai.yaml"
            policy_text = policy_file.read_text() if policy_file.is_file() else ""
            policy = re.search(r"(?m)^\s*allow_implicit_invocation:\s*(true|false)\s*$", policy_text)
            skills[key] = {
                "sha256": content_hash.hexdigest(),
                "files": len(files),
                "pixel_reference_lines": pixel_mentions,
                "allow_implicit_invocation": policy.group(1) == "true" if policy else None,
                "policy_file_present": policy_file.is_file(),
            }
    return skills


def collect_hook_sources(repo, codex_home):
    repo = Path(repo)
    codex_home = Path(codex_home)
    candidates = {
        "$CODEX_HOME/config.toml": codex_home / "config.toml",
        "$CODEX_HOME/hooks.json": codex_home / "hooks.json",
        "$CODEX_HOME/plugins/installed_plugins.json": codex_home / "plugins" / "installed_plugins.json",
        "repo:.codex/config.toml": repo / ".codex" / "config.toml",
        "repo:.codex/hooks.json": repo / ".codex" / "hooks.json",
        "repo:.codex/plugin-hooks.json": repo / ".codex" / "plugin-hooks.json",
    }
    for label, root in (("$CODEX_HOME/plugins", codex_home / "plugins"),
                        ("repo:.codex/plugins", repo / ".codex" / "plugins")):
        if root.is_dir():
            for path in sorted(root.rglob("*")):
                if path.is_file() and path.name in {
                    "plugin.json", "hooks.json", "plugin-hooks.json",
                }:
                    candidates[f"{label}/{path.relative_to(root).as_posix()}"] = path
    return {
        label: sha256(path.read_text())
        for label, path in candidates.items()
        if path.is_file()
    }


def pixel_reference_lines(repo, codex_home, developer_instructions):
    texts = [developer_instructions]
    codex_home = Path(codex_home)
    global_agents = codex_home / "AGENTS.md"
    if global_agents.is_file():
        texts.append(global_agents.read_text())
    repo = Path(repo)
    for path in repo.rglob("AGENTS.md"):
        if {".git", "node_modules", "target", ".next", "dist", "build"}.intersection(
            path.relative_to(repo).parts
        ):
            continue
        texts.append(path.read_text())
    for path in (codex_home / "config.toml", codex_home / "hooks.json",
                 codex_home / "plugins" / "installed_plugins.json",
                 repo / ".codex" / "config.toml", repo / ".codex" / "hooks.json",
                 repo / ".codex" / "plugin-hooks.json"):
        if path.is_file():
            texts.append(path.read_text())
    for root in (codex_home / "plugins", repo / ".codex" / "plugins"):
        if root.is_dir():
            for path in root.rglob("*"):
                if path.is_file() and path.name in {
                    "plugin.json", "hooks.json", "plugin-hooks.json",
                }:
                    texts.append(path.read_text())
    return sum(
        1 for text in texts for line in text.splitlines()
        if "pixel" in line.casefold()
    )


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
        "skills": collect_skills(repo, codex_home),
        "hook_config_files": collect_hook_sources(repo, codex_home),
        "pixel_reference_lines": pixel_reference_lines(
            repo, codex_home, developer_instructions
        ),
        "sources": [
            "Codex developer_instructions", "global/project AGENTS.md",
            "discoverable Codex skills", "known Codex hook/plugin configuration files",
        ],
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", required=True)
    parser.add_argument("--codex-home", required=True)
    args = parser.parse_args()
    print(json.dumps(collect_manifest(args.repo, args.codex_home), sort_keys=True))


if __name__ == "__main__":
    main()
