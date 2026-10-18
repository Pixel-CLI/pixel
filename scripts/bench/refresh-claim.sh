#!/usr/bin/env bash
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

# Re-run one public claim of docs/bench/claims.toml and keep its receipt.
#
# Runs the claim's `command` in a scratch directory (a leading `scripts/`
# resolves to this checkout's), with the `pixel` on PATH or $PIXEL_BIN, and
# writes docs/bench/receipts/<id>-<version>.txt: the claim, `pixel
# --version`, the date, the command, its exit status and its whole output
# (the scratch path written as `<scratch>`).
# The numbers in the claim's doc and the registry's binary_version, commit
# and measured fields are then updated by hand from that receipt, in the
# same pull request.
#
# A claim marked `live = true` needs an agent, a model or another installed
# or paid tool; it runs only with --live.
#
# usage: scripts/bench/refresh-claim.sh <id> [--live]
set -euo pipefail

root=$(cd "$(dirname "$0")/../.." && pwd -P)
if [ $# -lt 1 ] || [ $# -gt 2 ] || { [ $# -eq 2 ] && [ "$2" != --live ]; }; then
  echo "usage: refresh-claim.sh <id> [--live]" >&2
  exit 2
fi
id=$1
live_ok=${2:-}

fields=$(python3 - "$root/docs/bench/claims.toml" "$id" <<'EOF'
import sys, tomllib, shlex
claims = tomllib.load(open(sys.argv[1], "rb")).get("claim", [])
hit = [c for c in claims if c.get("id") == sys.argv[2]]
if not hit:
    sys.exit(f"refresh-claim: no claim {sys.argv[2]!r}; ids: {', '.join(c['id'] for c in claims)}")
c = hit[0]
print(f"command={shlex.quote(c['command'])}")
print(f"live={'1' if c['live'] else '0'}")
EOF
)
eval "$fields"
if [ "$live" = 1 ] && [ "$live_ok" != --live ]; then
  echo "refresh-claim: $id is a live claim (agent, model or third-party tool); pass --live to run: $command" >&2
  exit 3
fi

pixel=${PIXEL_BIN:-$(command -v pixel || true)}
if [ -z "$pixel" ]; then
  echo "refresh-claim: no pixel on PATH (set PIXEL_BIN)" >&2
  exit 1
fi
version_line=$("$pixel" --version | head -1)
version=${version_line#pixel }
receipt="$root/docs/bench/receipts/$id-$version.txt"
mkdir -p "$(dirname "$receipt")"

# Physical path: pixel prints canonical paths (macOS's /var is /private/var).
scratch=$(cd "$(mktemp -d "${TMPDIR:-/tmp}/pixel-claim.XXXXXX")" && pwd -P)
trap 'rm -rf "$scratch"' EXIT
case "$command" in
  scripts/*) run="$root/$command" ;;
  *) run=$command ;;
esac

set +e
output=$(cd "$scratch" && PATH="$(dirname "$pixel"):$PATH" PIXEL_BIN="$pixel" bash -c "$run" 2>&1)
status=$?
set -e
{
  echo "claim: $id"
  echo "date: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
  echo "command: $command"
  echo "exit: $status"
  echo "--- pixel --version"
  "$pixel" --version
  echo "--- output"
  # The scratch directory is machine-local: a receipt carries no local path.
  printf '%s\n' "${output//"$scratch"/<scratch>}"
} >"$receipt"
echo "receipt: ${receipt#"$root"/} (exit $status)"
exit "$status"
