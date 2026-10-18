#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Fail when a public number has outlived the binary that produced it.

Reads docs/bench/claims.toml (its header documents every field) and the
version of crates/pixel/Cargo.toml. A claim fails when:

- its `binary_version` is more than one minor release behind the crate, or
  "unknown", and it names no `tracking` issue that re-measures it;
- its `doc` no longer holds its `anchor` (the sentence with the number) or
  its `version_text` (where the doc states the version);
- a field is missing or malformed, or two claims share an `id`.

A stale claim with a `tracking` issue passes, printed as a warning, so the
debt stays visible on every run until a refresh lands.

usage: check-claims.py [--root DIR]
"""
from __future__ import annotations

import argparse
import re
import sys
import tomllib
from pathlib import Path

REQUIRED = ("id", "doc", "anchor", "version_text", "protocol", "binary_version",
            "commit", "measured", "command", "live")
VERSION = re.compile(r"^(\d+)\.(\d+)\.(\d+|x)$")


def crate_version(root: Path) -> tuple[int, int]:
    manifest = tomllib.loads((root / "crates/pixel/Cargo.toml").read_text())
    major, minor, _ = manifest["package"]["version"].split(".", 2)
    return int(major), int(minor)


def minors_behind(claimed: str, current: tuple[int, int]) -> int | None:
    """How many minor releases `claimed` trails `current`; None if unknown."""
    match = VERSION.match(claimed)
    if not match:
        return None
    major, minor = int(match.group(1)), int(match.group(2))
    if major != current[0]:
        return None if major > current[0] else 99
    return current[1] - minor


def check(root: Path) -> tuple[list[str], list[str]]:
    """The errors and the warnings of the registry under `root`."""
    errors: list[str] = []
    warnings: list[str] = []
    current = crate_version(root)
    claims = tomllib.loads((root / "docs/bench/claims.toml").read_text()).get("claim", [])
    if not claims:
        errors.append("docs/bench/claims.toml lists no [[claim]]")
    seen: set[str] = set()
    for claim in claims:
        cid = claim.get("id", "<no id>")
        missing = [f for f in REQUIRED if f not in claim]
        if missing:
            errors.append(f"{cid}: missing {', '.join(missing)}")
            continue
        if cid in seen:
            errors.append(f"{cid}: duplicate id")
        seen.add(cid)
        doc = root / claim["doc"]
        if not doc.is_file():
            errors.append(f"{cid}: {claim['doc']} does not exist")
            continue
        text = doc.read_text()
        for field in ("anchor", "version_text"):
            if claim[field] not in text:
                errors.append(f"{cid}: {claim['doc']} no longer contains its {field} {claim[field]!r}")
        version = claim["binary_version"]
        if version != "unknown" and not VERSION.match(version):
            errors.append(f"{cid}: binary_version {version!r} is not x.y.z, x.y.x or unknown")
            continue
        behind = minors_behind(version, current)
        if behind is not None and behind <= 1:
            continue
        what = (f"{cid}: {claim['doc']} quotes pixel {version}, "
                f"{'an unrecorded version' if behind is None else f'{behind} minor releases'} "
                f"behind {current[0]}.{current[1]}")
        if claim.get("tracking"):
            warnings.append(f"{what}; re-measure tracked in {claim['tracking']}: {claim['command']}")
        else:
            errors.append(f"{what}; re-measure it ({claim['command']}) or name a tracking issue")
    return errors, warnings


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parents[2])
    args = parser.parse_args()
    errors, warnings = check(args.root)
    for line in warnings:
        print(f"warning: {line}")
    for line in errors:
        print(f"error: {line}", file=sys.stderr)
    claims = len(tomllib.loads((args.root / "docs/bench/claims.toml").read_text()).get("claim", []))
    print(f"{claims} claims: {len(errors)} error(s), {len(warnings)} tracked stale")
    return 1 if errors else 0


if __name__ == "__main__":
    sys.exit(main())
