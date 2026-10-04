#!/bin/bash
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

# d2-5 — abstention rate of a whole-change negative, per motif.
# Usage: run.sh <name> <tree> <n>   (then: aggregate.py <name>=<clone> ...)
#   <tree> is a checkout this script may move (a dedicated worktree or clone);
#   it must have no tracked edits, and is put back where it started (branch or
#   commit) on exit, interrupted or not.
# For each of the last <n> non-merge commits of HEAD, oldest first: check the
# commit out, then `what-changed --base <commit>^ --json`, which refreshes the
# graph to the working tree and maps the commit's diff onto it.
set -u
name=$1 tree=$2 n=$3
BIN=${PIXEL_BIN:-pixel}
. "$(dirname "$0")/common.sh"
out=$(out_dir "$name")
mkdir -p "$out"
cd "$tree" || exit 1
require_clean || exit 1
origin=$(start_state)
trap 'restore "$origin"; "$BIN" daemon stop . >/dev/null 2>&1' EXIT
# Without these, a signal kills the shell before the EXIT trap runs.
trap 'exit 130' INT TERM HUP
tip=$(git rev-parse HEAD)
# --reverse applies after -n: the last <n> commits, oldest first; a root
# commit has no parent to diff against.
git rev-list --no-merges --min-parents=1 --reverse -n "$n" "$tip" > "$out/commits.txt"
[ -s "$out/commits.txt" ] || { echo "no commits selected from $tip" >&2; exit 1; }
i=0
while read -r c; do
  i=$((i + 1))
  measure "$out" "$out/errors.txt" "$i" "$c"
done < "$out/commits.txt"
echo "done $name"
