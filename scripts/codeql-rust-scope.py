#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Skip Rust only for a known non-Rust PR diff; uncertainty means a full scan."""

import argparse
from pathlib import Path, PurePosixPath
import subprocess
import sys


def affects_rust(filename):
    path = PurePosixPath(filename)
    # Changes to selection/extraction policy must exercise that policy in CI.
    if (filename == ".github/workflows/codeql.yml"
            or filename.startswith(".github/codeql/")
            or filename in {"scripts/codeql-rust-scope.py", "scripts/test-codeql-rust-scope.py"}):
        return True
    # Crate assets can be embedded by include_str!/include_bytes! or build.rs.
    if (path.suffix == ".rs" or filename.startswith(("crates/", "fuzz/"))
            or ".cargo" in path.parts
            or path.name in {"Cargo.toml", "Cargo.lock", "rust-toolchain", "rust-toolchain.toml"}):
        return True
    # Only known non-Rust inputs may skip. New/unknown kinds scan by default.
    return not (
        filename.startswith(("docs/", "website/", "changelog.d/", ".agents/", ".github/"))
        or path.suffix in {".md", ".rst", ".txt", ".py", ".pyi", ".pyw",
                           ".js", ".jsx", ".mjs", ".cjs", ".ts", ".tsx", ".mts", ".cts"}
        or path.name in {"LICENSE", "COPYING", "NOTICE"}
    )


def should_scan(event, root=Path(".")):
    if event != "pull_request":
        return True, "Non-PR events always scan all Rust sources."
    try:
        # checkout fetch-depth: 2 includes the synthetic merge and both parents.
        # Its first parent is the actual target tree used by CodeQL, not an
        # older merge base or a branch name that could have moved since dispatch.
        parents = subprocess.check_output(
            ["git", "rev-list", "--parents", "-n", "1", "HEAD"], cwd=root
        ).split()
        if len(parents) != 3:
            return True, "No two-parent PR merge commit; scanning conservatively."
        changed = subprocess.check_output(
            ["git", "diff", "--name-only", "--no-renames", "-z", "HEAD^1", "HEAD"], cwd=root
        )
    except (OSError, subprocess.CalledProcessError):
        return True, "Cannot read the PR diff; scanning conservatively."
    # --no-renames retains both old and new paths, including removed Rust files.
    paths = [name.decode("utf-8", errors="surrogateescape") for name in changed.split(b"\0") if name]
    run = any(affects_rust(name) for name in paths)
    return run, "Rust inputs changed or unknown inputs present." if run else "Only known non-Rust inputs changed."


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--event", required=True)
    args = parser.parse_args()
    run, reason = should_scan(args.event)
    print(reason, file=sys.stderr)
    print(f"run={str(run).lower()}")


if __name__ == "__main__":
    main()
