#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Stage one explicitly distributed skill into a disposable arena snapshot."""
import argparse
import hashlib
import json
import re
import shutil
from pathlib import Path


POLICY = re.compile(r"(?m)^(\s*allow_implicit_invocation:\s*)(true|false)(\s*(?:#.*)?)$")
NAME = re.compile(r"(?m)^name:\s*([A-Za-z0-9_-]+)\s*$")


def tree_hash(folder):
    digest = hashlib.sha256()
    files = sorted(path for path in folder.rglob("*") if path.is_file())
    for path in files:
        if path.is_symlink():
            raise ValueError(f"skill package may not contain symlinks: {path}")
        digest.update(path.relative_to(folder).as_posix().encode())
        digest.update(b"\0")
        digest.update(path.read_bytes())
        digest.update(b"\0")
    return digest.hexdigest()


def inspect_source(source):
    source = Path(source).resolve(strict=True)
    skill_file = source / "SKILL.md"
    policy_file = source / "agents" / "openai.yaml"
    if not skill_file.is_file() or not policy_file.is_file():
        raise ValueError("candidate must contain SKILL.md and agents/openai.yaml")
    match = NAME.search(skill_file.read_text())
    if not match:
        raise ValueError("candidate SKILL.md must declare a simple name")
    policy_text = policy_file.read_text()
    policy_matches = list(POLICY.finditer(policy_text))
    if len(policy_matches) != 1 or policy_matches[0].group(2) != "false":
        raise ValueError("distributed candidate must explicitly set allow_implicit_invocation: false")
    return source, match.group(1), policy_text


def stage(source, repo, arm, receipt_path):
    source, name, source_policy = inspect_source(source)
    repo = Path(repo)
    receipt = {
        "skill_name": name,
        "arm": arm,
        "source_sha256": tree_hash(source),
        "source_policy": "explicit_only",
        "staged": False,
        "staged_sha256": None,
        "staged_policy": None,
    }
    if arm == "pixel":
        destination = repo / ".agents" / "skills" / name
        if destination.exists():
            raise ValueError(f"skill destination already exists: {destination}")
        shutil.copytree(source, destination, symlinks=False)
        candidate_policy = POLICY.sub(r"\1true\3", source_policy, count=1)
        (destination / "agents" / "openai.yaml").write_text(candidate_policy)
        receipt.update({
            "staged": True,
            "staged_sha256": tree_hash(destination),
            "staged_policy": "implicit_invocation_enabled_for_experiment",
        })
    elif arm != "raw":
        raise ValueError(f"unsupported arm for skill staging: {arm}")
    Path(receipt_path).write_text(json.dumps(receipt, indent=2) + "\n")
    return receipt


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)
    inspect_parser = subparsers.add_parser("inspect")
    inspect_parser.add_argument("--source", required=True)
    stage_parser = subparsers.add_parser("stage")
    stage_parser.add_argument("--source", required=True)
    stage_parser.add_argument("--repo", required=True)
    stage_parser.add_argument("--arm", required=True, choices=("raw", "pixel"))
    stage_parser.add_argument("--receipt", required=True)
    args = parser.parse_args()
    if args.command == "inspect":
        source, name, _ = inspect_source(args.source)
        print(json.dumps({"skill_name": name, "source_sha256": tree_hash(source)}, sort_keys=True))
    else:
        receipt = stage(args.source, args.repo, args.arm, args.receipt)
        print(json.dumps(receipt, sort_keys=True))


if __name__ == "__main__":
    main()
