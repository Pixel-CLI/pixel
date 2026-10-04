#!/usr/bin/env python3
"""Fail when deny.toml and osv-scanner.toml accept different advisories.

cargo-deny reads `[advisories] ignore` in deny.toml; OpenSSF Scorecard's
Vulnerabilities check runs osv-scanner, which reads `[[IgnoredVulns]]` in
osv-scanner.toml and never deny.toml. An advisory accepted in one file and
not the other is either a Scorecard finding for a decision already made, or
an acceptance cargo-deny never reviewed. Every osv-scanner entry must also
carry a reason, as deny.toml's do.

Usage: check-advisory-ignores.py [DENY_TOML OSV_SCANNER_TOML]
(defaults: the two files at the repository root). Exit 0 when the lists
agree, 1 with one line per difference otherwise.
"""
from pathlib import Path
import sys
import tomllib

ROOT = Path(__file__).resolve().parent.parent


def deny_ids(path):
    """The advisory ids deny.toml ignores, in either the string or table form."""
    entries = tomllib.loads(path.read_text())["advisories"].get("ignore", [])
    return {entry if isinstance(entry, str) else entry["id"] for entry in entries}


def osv_entries(path):
    """osv-scanner.toml's ignored ids mapped to their reason ('' when missing)."""
    entries = tomllib.loads(path.read_text()).get("IgnoredVulns", [])
    # An entry without an id ignores nothing for osv-scanner; it surfaces
    # below as an advisory deny.toml does not accept.
    return {entry.get("id", "<entry without id>"): entry.get("reason", "").strip()
            for entry in entries}


def problems(deny, osv):
    found = []
    for advisory in sorted(deny - osv.keys()):
        found.append(f"{advisory}: ignored in deny.toml, missing from osv-scanner.toml")
    for advisory in sorted(osv.keys() - deny):
        found.append(f"{advisory}: ignored in osv-scanner.toml, not accepted in deny.toml")
    for advisory, reason in sorted(osv.items()):
        if not reason:
            found.append(f"{advisory}: osv-scanner.toml entry has no reason")
    return found


def main(argv):
    if len(argv) not in (1, 3):
        print("usage: check-advisory-ignores.py [DENY_TOML OSV_SCANNER_TOML]", file=sys.stderr)
        return 2
    deny_path, osv_path = (Path(argv[1]), Path(argv[2])) if len(argv) == 3 else (
        ROOT / "deny.toml", ROOT / "osv-scanner.toml")
    deny, osv = deny_ids(deny_path), osv_entries(osv_path)
    found = problems(deny, osv)
    for line in found:
        print(line, file=sys.stderr)
    if found:
        return 1
    print(f"{len(deny)} advisory ignore(s) agree between deny.toml and osv-scanner.toml")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
