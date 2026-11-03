#!/bin/bash
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

# Re-run the commits that run.sh could not measure (errors.txt). Before #307
# the pixel under test appended `.pixel/` to a tracked .gitignore that lacked
# it, and that edit blocked the next checkout; measure now undoes it after
# each run.
# errors.txt stays in place until every entry has been retried, then is
# replaced by the entries that failed again (or removed): an interrupted
# rerun leaves the full pending list, never a shortened one.
# Usage: rerun.sh <name> <tree>
set -u
name=$1 tree=$2
BIN=${PIXEL_BIN:-pixel}
. "$(dirname "$0")/common.sh"
out=$(out_dir "$name")
[ -s "$out/errors.txt" ] || exit 0
cd "$tree" || exit 1
require_clean || exit 1
origin=$(start_state)
trap 'restore "$origin"; "$BIN" daemon stop . >/dev/null 2>&1' EXIT
# Without these, a signal kills the shell before the EXIT trap runs.
trap 'exit 130' INT TERM HUP
next="$out/errors.next.txt"
: > "$next"
while read -r i c _; do
  measure "$out" "$next" "$i" "$c" rerun
done < "$out/errors.txt"
if [ -s "$next" ]; then mv "$next" "$out/errors.txt"; else rm -f "$next" "$out/errors.txt"; fi
echo "rerun done $name"
