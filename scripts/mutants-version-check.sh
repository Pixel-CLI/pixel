#!/bin/sh
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

# Refuses a local cargo-mutants other than the version the Mutants workflow
# pins. Two releases can generate different mutants for the same diff, and
# classify the same outcome differently, so a local run on another version
# can be green where CI is red on the same commit. The pin lives in
# .github/workflows/mutants.yml (`tool: cargo-mutants@X.Y.Z`, once per job
# that installs it); this script reads it there, never a copy of its own.
#
#   scripts/mutants-version-check.sh <repo root>
set -eu

repo=${1:?usage: scripts/mutants-version-check.sh <repo root>}
workflow="$repo/.github/workflows/mutants.yml"
pin=$(sed -n 's/.*tool: cargo-mutants@\([^ ]*\).*/\1/p' "$workflow" 2>/dev/null | sort -u)
case "$pin" in
    ""|*[!0-9.]*)
        echo "mutation exposure: cannot read one cargo-mutants pin from $workflow (got '$pin')" >&2
        exit 2
        ;;
esac
installed=$(cargo mutants --version 2>/dev/null | awk '{print $2}') || installed=
if [ "$installed" != "$pin" ]; then
    echo "mutation exposure: local cargo-mutants is '${installed:-missing}', CI runs $pin;" \
        "install it with: cargo install cargo-mutants --locked --version $pin" >&2
    exit 2
fi
