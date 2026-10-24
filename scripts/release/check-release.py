#!/usr/bin/env python3
# SPDX-FileCopyrightText: The RTO contributors
# SPDX-License-Identifier: MIT

"""check-release: the consistency a release tag must have before anything is
built or published (`.agents/skills/release/SKILL.md`).

    scripts/release/check-release.py v0.1.0 [--repo .] [--json]

Three drifts have each produced a green tag and a broken release elsewhere:

1. the tag names a version that `crates/pixel/Cargo.toml` does not carry;
2. `Cargo.lock` is stale for a workspace member (a version was bumped without
   a `cargo build`), so `cargo build --locked` fails on the release runner,
   after the tests already passed;
3. `CHANGELOG.md` has no `## [x.y.z]` heading, or the release notes are still
   filed under `## [Unreleased]`, so the GitHub release body is a fallback.

Every check is a pure function over file contents; `run` only reads the files
and assembles the report. Exit 0: every check passed; 1: a check failed;
2: a file cannot be read or the version is malformed.
"""

import argparse
import glob
import json
import re
import sys
import tomllib
from pathlib import Path

CLI_MANIFEST = "crates/pixel/Cargo.toml"


def normalize_version(text):
    """`1.2.3`, `v1.2.3` and `refs/tags/v1.2.3` give `1.2.3` (a pre-release
    suffix such as `-rc.1` is kept); anything else gives None."""
    value = text.strip().removeprefix("refs/tags/").removeprefix("v")
    core = value.split("-", 1)[0]
    parts = core.split(".")
    if len(parts) != 3 or any(not part.isascii() or not part.isdigit() for part in parts):
        return None
    return value


def package_name_version(manifest):
    """`(name, version)` from a manifest's `[package]` table, or None. A
    `version.workspace = true` has no version of its own: None."""
    try:
        package = tomllib.loads(manifest).get("package", {})
    except tomllib.TOMLDecodeError:
        return None
    name, version = package.get("name"), package.get("version")
    if isinstance(name, str) and isinstance(version, str):
        return name, version
    return None


def workspace_members(root_manifest, repo):
    """The member directories of the root manifest's `members` list, globs
    expanded, sorted."""
    try:
        members = tomllib.loads(root_manifest).get("workspace", {}).get("members", [])
    except tomllib.TOMLDecodeError:
        return []
    found = []
    for member in members:
        if any(ch in member for ch in "*?["):
            found += sorted(
                str(Path(path).relative_to(repo))
                for path in glob.glob(str(Path(repo) / member))
                if (Path(path) / "Cargo.toml").is_file()
            )
        else:
            found.append(member)
    return found


def lock_packages(lock):
    """Every `[[package]]` of a Cargo.lock as `(name, version)`."""
    try:
        packages = tomllib.loads(lock).get("package", [])
    except tomllib.TOMLDecodeError:
        return []
    return [(p["name"], p["version"]) for p in packages if "name" in p and "version" in p]


def check_cli_version(cli_manifest, expected):
    found = package_name_version(cli_manifest)
    if found is None:
        return "cli-version", False, f"no [package] name/version in {CLI_MANIFEST}"
    name, version = found
    if version == expected:
        return "cli-version", True, f"{name} {version} matches"
    return (
        "cli-version",
        False,
        f"{name} is {version}, tag says {expected}; bump `version` in {CLI_MANIFEST} or tag v{version}",
    )


def check_lock(lock, members):
    """Cargo.lock must carry every workspace member at its manifest version:
    a stale entry makes `cargo build --locked` fail on the release runner."""
    packages = lock_packages(lock)
    stale = []
    for name, version in members:
        locked = [v for n, v in packages if n == name]
        if not locked:
            stale.append(f"{name} missing from Cargo.lock")
        elif version not in locked:
            stale.append(f"{name} {locked[0]} (manifest {version})")
    if not stale:
        return "cargo-lock", True, f"{len(members)} workspace members at their manifest version"
    return (
        "cargo-lock",
        False,
        f"stale: {', '.join(stale)}; run `cargo build` (or `cargo update --workspace`) and commit Cargo.lock",
    )


def check_changelog(changelog, version):
    """CHANGELOG.md must have a `## [x.y.z]` heading and an empty
    `## [Unreleased]`: the release body is cut from the heading, and notes
    left under Unreleased would be silently dropped from it."""
    heading = f"## [{version}]"
    has_heading, in_unreleased, entries = False, False, 0
    for line in changelog.splitlines():
        if line.startswith("## "):
            in_unreleased = line.startswith("## [Unreleased]")
            rest = line[len(heading):] if line.startswith(heading) else None
            if rest is not None and (rest == "" or rest[0] == " "):
                has_heading = True
        elif in_unreleased and line.lstrip().startswith("- "):
            entries += 1
    noun = "entry" if entries == 1 else "entries"
    if has_heading and entries == 0:
        return "changelog", True, f"{heading} present, Unreleased empty"
    if has_heading:
        return (
            "changelog",
            False,
            f"{heading} present but {entries} {noun} still under ## [Unreleased]; move them under the release heading",
        )
    if entries:
        return (
            "changelog",
            False,
            f"no {heading} heading; {entries} {noun} under ## [Unreleased] — cut the release section first",
        )
    return "changelog", False, f"no {heading} heading in CHANGELOG.md"


def run(repo, version_input):
    version = normalize_version(version_input)
    if version is None:
        raise ValueError(
            f"`{version_input}` is not a version (expected x.y.z, vx.y.z or refs/tags/vx.y.z)"
        )
    repo = Path(repo)

    def read(rel):
        try:
            return (repo / rel).read_text()
        except OSError as error:
            raise ValueError(f"{rel}: {error.strerror or error}") from error

    members = []
    for member in workspace_members(read("Cargo.toml"), repo):
        found = package_name_version(read(f"{member}/Cargo.toml"))
        if found is None:
            raise ValueError(f"{member}/Cargo.toml: no [package] name/version")
        members.append(found)
    checks = [
        check_cli_version(read(CLI_MANIFEST), version),
        check_lock(read("Cargo.lock"), members),
        check_changelog(read("CHANGELOG.md"), version),
    ]
    return version, checks


def render(version, checks):
    lines = [f"check-release {version}"]
    lines += [f"[{'ok  ' if ok else 'FAIL'}] {name}: {detail}" for name, ok, detail in checks]
    lines.append(
        "check-release: all checks passed"
        if all(ok for _, ok, _ in checks)
        else "check-release: FAILED"
    )
    return "\n".join(lines) + "\n"


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("version", help="x.y.z, vx.y.z or refs/tags/vx.y.z")
    parser.add_argument("--repo", default=".", help="the repository root")
    parser.add_argument("--json", action="store_true", help="print the report as JSON")
    args = parser.parse_args(argv)
    try:
        version, checks = run(args.repo, args.version)
    except ValueError as error:
        print(f"check-release: {error}", file=sys.stderr)
        return 2
    ok = all(good for _, good, _ in checks)
    if args.json:
        print(
            json.dumps(
                {
                    "version": version,
                    "ok": ok,
                    "checks": [{"name": n, "ok": g, "detail": d} for n, g, d in checks],
                },
                indent=2,
            )
        )
    else:
        sys.stdout.write(render(version, checks))
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
