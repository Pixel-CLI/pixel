#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Fail when deny.toml and osv-scanner.toml accept different advisories.

cargo-deny reads `[advisories] ignore` in deny.toml; OpenSSF Scorecard's
Vulnerabilities check runs osv-scanner, which reads `[[IgnoredVulns]]` in
osv-scanner.toml and never deny.toml. An advisory accepted in one file and
not the other is either a Scorecard finding for a decision already made, or
an acceptance cargo-deny never reviewed. Every entry in either file carries
a reason (so deny.toml's bare-string form is refused) and names its id once.

Usage: check-advisory-ignores.py [DENY_TOML OSV_SCANNER_TOML]
(defaults: the two files at the repository root). Exit 0 when the lists
agree, 1 with one line per difference otherwise.
"""
from pathlib import Path
import sys
import tomllib

ROOT = Path(__file__).resolve().parent.parent


def accepted(label, entries):
    """The ids a file accepts, and one problem line per entry that breaks the policy.

    Each entry is judged on its own before the ids are reduced to a set, so a
    duplicate cannot hide a sibling without a reason. deny.toml also admits a
    bare-string entry, which has no room for a reason and is refused.
    """
    ids, found = set(), []
    for entry in entries:
        if isinstance(entry, str):
            advisory, reason = entry, ""
        else:
            advisory, reason = entry.get("id", ""), str(entry.get("reason", "")).strip()
        if not advisory:
            found.append(f"{label}: an entry has no id")
            continue
        if advisory in ids:
            found.append(f"{advisory}: listed twice in {label}")
        if not reason:
            found.append(f"{advisory}: {label} entry has no reason")
        ids.add(advisory)
    return ids, found


def problems(deny_toml, osv_toml):
    deny, found = accepted("deny.toml", deny_toml["advisories"].get("ignore", []))
    osv, osv_found = accepted("osv-scanner.toml", osv_toml.get("IgnoredVulns", []))
    found += osv_found
    for advisory in sorted(deny - osv):
        found.append(f"{advisory}: ignored in deny.toml, missing from osv-scanner.toml")
    for advisory in sorted(osv - deny):
        found.append(f"{advisory}: ignored in osv-scanner.toml, not accepted in deny.toml")
    return deny, found


def main(argv):
    if len(argv) not in (1, 3):
        print("usage: check-advisory-ignores.py [DENY_TOML OSV_SCANNER_TOML]", file=sys.stderr)
        return 2
    deny_path, osv_path = (Path(argv[1]), Path(argv[2])) if len(argv) == 3 else (
        ROOT / "deny.toml", ROOT / "osv-scanner.toml")
    deny, found = problems(tomllib.loads(deny_path.read_text()), tomllib.loads(osv_path.read_text()))
    for line in found:
        print(line, file=sys.stderr)
    if found:
        return 1
    print(f"{len(deny)} advisory ignore(s) agree between deny.toml and osv-scanner.toml")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
