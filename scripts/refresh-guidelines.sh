#!/bin/sh
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

# Refresh the vendored Microsoft Pragmatic Rust Guidelines and show what moved.
#
#   scripts/refresh-guidelines.sh
#
# Re-downloads https://microsoft.github.io/rust-guidelines/agents/all.txt over
# .agents/skills/rust-guidelines/guidelines.txt, then prints the diff of the
# `## <title> (M-ID)` headings between the old and the new text: an added,
# renamed or removed upstream rule shows up here, and the docs-drift test
# (crates/pixel/tests/cli/docs_drift.rs) fails the build if SKILL.md still
# names an id the new text no longer has. Exit status: 0 when the headings
# are unchanged, 1 when they differ (the body may differ either way; look
# at `git diff` for that), 2 on a download failure (the old file is kept).
set -eu

URL="https://microsoft.github.io/rust-guidelines/agents/all.txt"
ROOT=$(cd "$(dirname "$0")/.." && pwd)
TARGET="$ROOT/.agents/skills/rust-guidelines/guidelines.txt"
TMP=$(mktemp)
trap 'rm -f "$TMP" "$TMP.old" "$TMP.new"' EXIT

headings() {
    grep '^## .*(M-[A-Z0-9-]*)' "$1" | sed 's/ { #M-[A-Z0-9-]* }$//' | sort
}

if ! curl -fsSL "$URL" -o "$TMP"; then
    echo "refresh-guidelines: download failed, $TARGET left untouched" >&2
    exit 2
fi
if ! grep -q '^## .*(M-' "$TMP"; then
    echo "refresh-guidelines: downloaded text has no (M-…) headings, $TARGET left untouched" >&2
    exit 2
fi

headings "$TARGET" > "$TMP.old"
headings "$TMP" > "$TMP.new"
cp "$TMP" "$TARGET"

OLD_COUNT=$(wc -l < "$TMP.old" | tr -d ' ')
NEW_COUNT=$(wc -l < "$TMP.new" | tr -d ' ')
echo "guidelines.txt refreshed: $OLD_COUNT -> $NEW_COUNT rule headings"
if diff -u "$TMP.old" "$TMP.new"; then
    echo "rule headings unchanged"
    exit 0
fi
echo "rule headings changed: update SKILL.md, then run" >&2
echo "  cargo test -p pixel-cli --test cli docs_drift" >&2
exit 1
