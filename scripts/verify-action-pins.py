#!/usr/bin/env python3
"""Every `uses:` in the workflows names an immutable reference.

A tag or a branch is a pointer its owner can move: `actions/checkout@v7`
runs whatever `v7` names on the day of the run, not what was reviewed.
`release.yml` holds `HOMEBREW_TAP_TOKEN` and `release-build.yml`, which it
calls, an OIDC token that Sigstore turns into a signing certificate for this
repository, so a moved upstream tag there runs someone else's code with
those credentials. The workflows already pin every action to a full commit
SHA with a trailing `# vN` comment (Dependabot moves both together); this
check keeps it that way instead of relying on review.

Accepted, one per line:

- `uses: owner/repo[/path]@<40 lowercase hex> # <ref name>`: an action or a
  reusable workflow of another repository, pinned, with the human-readable
  ref it stands for (Dependabot needs it to propose the next bump);
- `uses: ./path`: a local action or workflow, versioned with this tree;
- `uses: docker://image@sha256:<64 hex>`: an image by digest.

Anything else fails, including valid YAML this line reader does not parse
(a quoted key, a flow mapping `- {uses: ...}`): an unusual form is reported
rather than guessed at, since a check that skips what it cannot read passes
over exactly the line it exists for. The trade is a false positive on a
`run:` script that prints a line starting with `uses:`, which none does.

Usage: python3 scripts/verify-action-pins.py [REPO]   (default: this checkout)
Exit 0 when every reference is pinned, 1 otherwise (each offender listed as
`path:line: reason`).
"""

from pathlib import Path
import re
import sys

#: Any line that could be a `uses` key: plain, quoted, after a list dash or
#: a flow-mapping brace. Deliberately wider than what `PINNED` accepts.
DIRECTIVE = re.compile(r"""^\s*(?:-\s+)?(?:\{\s*)?['"]?uses['"]?\s*:""")

#: `uses:` followed by the value and an optional comment, nothing else.
PLAIN = re.compile(r"^\s*(?:-\s+)?uses:\s+(?P<value>\S+)(?:\s+#\s*(?P<comment>.*?))?\s*$")

REMOTE = re.compile(r"^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+(?:/[A-Za-z0-9_./-]+)?@(?P<ref>.+)$")
FULL_SHA = re.compile(r"^[0-9a-f]{40}$")
DOCKER_DIGEST = re.compile(r"^docker://[^@\s]+@sha256:[0-9a-f]{64}$")


def workflow_files(repo: Path) -> list[Path]:
    """Workflows (GitHub runs both extensions) and local composite actions."""
    files = [
        *repo.glob(".github/workflows/*.yml"),
        *repo.glob(".github/workflows/*.yaml"),
        *repo.glob(".github/actions/**/action.yml"),
        *repo.glob(".github/actions/**/action.yaml"),
    ]
    return sorted(files)


def check_line(line: str) -> str | None:
    """The reason `line` is not an accepted reference, or None when it is
    one (or is not a `uses` line at all)."""
    if not DIRECTIVE.match(line):
        return None
    plain = PLAIN.match(line)
    if not plain:
        return "unusual `uses:` syntax (quoted key, flow mapping or trailing content); write it as `uses: owner/repo@<sha> # vX`"
    value, comment = plain.group("value"), plain.group("comment")
    if value.startswith("./"):
        return None
    if value.startswith("docker://"):
        return None if DOCKER_DIGEST.match(value) else "docker image not pinned by `@sha256:<digest>`"
    remote = REMOTE.match(value)
    if not remote:
        return f"unrecognised reference `{value}`"
    if not FULL_SHA.match(remote.group("ref")):
        return f"`@{remote.group('ref')}` is not a full 40-hex commit SHA"
    if not comment:
        return "pinned SHA without a `# <ref>` comment naming the version it stands for"
    return None


def offenders(repo: Path) -> list[str]:
    found = []
    for path in workflow_files(repo):
        for number, line in enumerate(path.read_text().splitlines(), start=1):
            reason = check_line(line)
            if reason:
                found.append(f"{path.relative_to(repo)}:{number}: {reason}")
    return found


def main(argv: list[str]) -> int:
    repo = Path(argv[1]) if len(argv) > 1 else Path(__file__).resolve().parent.parent
    files = workflow_files(repo)
    if not files:
        print(f"no workflow under {repo}/.github: nothing was checked", file=sys.stderr)
        return 1
    bad = offenders(repo)
    if bad:
        print("Action references not pinned to an immutable ref:", file=sys.stderr)
        for line in bad:
            print(f"  {line}", file=sys.stderr)
        print("Pin them like: uses: owner/repo@<40-hex-sha> # vX.Y.Z", file=sys.stderr)
        return 1
    print(f"{len(files)} workflow files: every `uses:` is pinned")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
