#!/usr/bin/env python3
# SPDX-FileCopyrightText: The RTO contributors
# SPDX-License-Identifier: MIT
"""Fail when a tracked source file lacks its SPDX copyright and license lines.

OpenSSF Best Practices (gold: copyright_per_file, license_per_file) asks for
a copyright and a license statement in each source file. New files in scope
carry, near their top (after a shebang when they have one):

    SPDX-FileCopyrightText: The Pixel contributors
    SPDX-License-Identifier: MIT

in its language's line comment. The scope is the code and build definitions
git tracks (`SUFFIXES`, `NAMES`, `EXTRA`), minus `EXCLUDED`, each entry with
the reason it is left out. RTO-attributed files present when the import
manifest was added keep their existing attribution; other files must use
Pixel attribution.

Usage: check-spdx.py [--fix] [ROOT]
(ROOT defaults to this repository). Exit 0 when every file in scope carries
both lines, 1 with one path per offender otherwise. `--fix` inserts the
header into each offender instead, and exits 0.
"""
from pathlib import Path
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[2]

COPYRIGHT = "SPDX-FileCopyrightText: The Pixel contributors"
HISTORICAL_COPYRIGHT = "SPDX-FileCopyrightText: The RTO contributors"
LICENSE = "SPDX-License-Identifier: MIT"
IMPORT_MANIFEST = "docs/imports/rto-pixel-2026-10-24.json"

# The comment leader for each suffix in scope.
SUFFIXES = {
    ".rs": "//",
    ".ts": "//",
    ".tsx": "//",
    ".js": "//",
    ".mjs": "//",
    ".py": "#",
    ".sh": "#",
}
# Files in scope by name, wherever they are.
NAMES = {"Cargo.toml": "#"}
# Files in scope by path prefix: CI definitions and extensionless hooks.
EXTRA = {
    ".github/workflows/": "#",
    ".github/actions/": "#",
    ".githooks/": "#",
}

# Path prefixes left out, and why.
EXCLUDED = {
    # Written verbatim into users' agent configurations by `pixel setup`
    # (the pi guard extension, the OpenCode plugin): a header there changes
    # installed content. The root LICENSE covers them.
    "crates/pixel-setup/assets/",
    # Vendored from an upstream skill under its own license (see UPSTREAM).
    ".agents/skills/improve-codebase-architecture/",
}

# How far into a file the header may sit: a shebang, then the two lines.
HEAD_LINES = 6


def leader(path):
    """The line-comment leader when `path` is in scope, else None."""
    if any(path.startswith(prefix) for prefix in EXCLUDED):
        return None
    name = path.rsplit("/", 1)[-1]
    if name in NAMES:
        return NAMES[name]
    for prefix, mark in EXTRA.items():
        if path.startswith(prefix) and (
            prefix == ".githooks/" or path.endswith((".yml", ".yaml"))
        ):
            return mark
    suffix = name[name.rfind("."):] if "." in name else ""
    return SUFFIXES.get(suffix)


def tracked(root):
    out = subprocess.run(
        ["git", "-C", str(root), "ls-files", "-z"],
        check=True,
        capture_output=True,
    ).stdout
    return [p for p in out.decode().split("\0") if p]


def historical_rto_paths(root):
    commits = subprocess.run(
        [
            "git", "-C", str(root), "log", "--diff-filter=A", "--reverse",
            "--format=%H", "--", IMPORT_MANIFEST,
        ],
        check=True,
        capture_output=True,
        text=True,
    ).stdout.splitlines()
    if not commits:
        raise RuntimeError(
            f"cannot find the commit that introduced {IMPORT_MANIFEST}; "
            "the Git history must include the import"
        )
    out = subprocess.run(
        [
            "git", "-C", str(root), "grep", "-Il", "-z",
            HISTORICAL_COPYRIGHT, commits[0], "--",
        ],
        check=True,
        capture_output=True,
    ).stdout
    return {p.split(":", 1)[1] for p in out.decode().split("\0") if p}


def has_header(text):
    head = text.splitlines()[:HEAD_LINES]
    return any(COPYRIGHT in line for line in head) and any(
        LICENSE in line for line in head
    )


def with_header(text, mark):
    """`text` with the two header lines inserted after any shebang."""
    header = f"{mark} {COPYRIGHT}\n{mark} {LICENSE}\n"
    lines = text.splitlines(keepends=True)
    # A shebang stays first; a Rust inner attribute (`#![...]`) is not one.
    keep = 1 if lines and lines[0].startswith("#!") and not lines[0].startswith("#![") else 0
    rest = "".join(lines[keep:])
    sep = "" if not rest or rest.startswith("\n") else "\n"
    return "".join(lines[:keep]) + header + sep + rest


def main(argv):
    fix = "--fix" in argv
    args = [a for a in argv if a != "--fix"]
    root = Path(args[0]).resolve() if args else ROOT
    missing = []
    try:
        historical = historical_rto_paths(root)
    except (OSError, subprocess.CalledProcessError, RuntimeError) as error:
        print(f"cannot determine historical SPDX paths: {error}", file=sys.stderr)
        return 1
    for path in tracked(root):
        mark = leader(path)
        if mark is None:
            continue
        file = root / path
        if not file.is_file() or file.is_symlink():
            continue
        text = file.read_text(encoding="utf-8")
        if has_header(text) or (
            path in historical
            and has_header(text.replace(HISTORICAL_COPYRIGHT, COPYRIGHT, 1))
        ):
            continue
        if fix:
            file.write_text(with_header(text, mark), encoding="utf-8")
        else:
            missing.append(path)
    for path in missing:
        print(f"{path}: missing the SPDX header ({COPYRIGHT} / {LICENSE})")
    if missing:
        print(f"{len(missing)} file(s); run scripts/ci/check-spdx.py --fix", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
