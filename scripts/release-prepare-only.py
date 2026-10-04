#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Exit 0 when a diff is only what `prepare.sh` writes, 1 otherwise.

`ci.yml`, `mutants.yml` and `cross-build.yml` skip their jobs on a
release-prepare pull request into main (the push run on its merge commit
tests the same tree before the tag). A branch name alone cannot say that a
pull request is one: any code change pushed on a `release-` branch would
skip every gate. This script checks the diff itself, so a skip needs both.

Allowed, and nothing else:
- the version line of each `crates/*/Cargo.toml`, of `Cargo.lock` and of
  the plugin manifests (`pixel_release::PLUGIN_MANIFESTS`);
- `CHANGELOG.md`, any line;
- the deletion of a `changelog.d/*.md` fragment.

A `Cargo.lock` dependency bump changes its `checksum` line too, so it fails
the version-line rule and keeps the gates.

Usage: release-prepare-only.py <base> [<head>]
  With <head>, diffs base...head (CI: the pull request). Without it, diffs
  <base> against the working tree, untracked files included (prepare.sh:
  `HEAD`, what it has just written before anything is committed).
"""

import re
import subprocess
import sys

VERSIONED = re.compile(
    r"crates/[^/]+/Cargo\.toml|Cargo\.lock|plugin\.yaml|package\.json"
    r"|gemini-extension\.json|\.(claude|codex|devin|qoder)-plugin/plugin\.json"
)
VERSION_LINE = re.compile(r'\s*"?version"?\s*[=:]\s*"?\d+\.\d+\.\d+"?,?\s*')
FRAGMENT = re.compile(r"changelog\.d/[^/]+\.md")


def violations(files, changed_lines):
    """Reasons the diff is not release-prepare only; empty when it is.

    `files` maps path to git status letter; `changed_lines` maps path to its
    added and removed lines, without the `+`/`-` marker."""
    reasons = []
    for path, status in sorted(files.items()):
        if path == "CHANGELOG.md":
            continue
        if FRAGMENT.fullmatch(path):
            if status != "D":
                reasons.append(f"{path}: a fragment may only be deleted ({status})")
            continue
        if not VERSIONED.fullmatch(path):
            reasons.append(f"{path}: not a file prepare.sh writes")
            continue
        if status != "M":
            reasons.append(f"{path}: prepare.sh only edits it ({status})")
            continue
        for line in changed_lines.get(path, []):
            if not VERSION_LINE.fullmatch(line):
                reasons.append(f"{path}: changes a line other than its version: {line!r}")
                break
    return reasons


def read_diff(base, head=None):
    """The diff base...head, or base against the working tree without head."""
    rng = base if head is None else f"{base}...{head}"
    status = subprocess.run(
        ["git", "diff", "--name-status", "--no-renames", rng],
        check=True, capture_output=True, text=True,
    ).stdout
    files = {}
    for row in status.splitlines():
        letter, path = row.split("\t", 1)
        files[path] = letter[0]
    if head is None:
        untracked = subprocess.run(
            ["git", "ls-files", "--others", "--exclude-standard"],
            check=True, capture_output=True, text=True,
        ).stdout
        for path in untracked.splitlines():
            files[path] = "A"
    patch = subprocess.run(
        ["git", "diff", "-U0", "--no-renames", rng],
        check=True, capture_output=True, text=True,
    ).stdout
    changed, current = {}, None
    for line in patch.splitlines():
        if line.startswith("+++ "):
            current = None if line == "+++ /dev/null" else line[6:]
        elif line.startswith("--- "):
            if line != "--- /dev/null":
                current = line[6:]
        elif current and line[:1] in "+-":
            changed.setdefault(current, []).append(line[1:])
    return files, changed


def main(argv):
    if len(argv) not in (2, 3):
        print(__doc__[__doc__.index("Usage:"):].strip(), file=sys.stderr)
        return 2
    reasons = violations(*read_diff(*argv[1:]))
    for reason in reasons:
        print(reason, file=sys.stderr)
    return 1 if reasons else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
