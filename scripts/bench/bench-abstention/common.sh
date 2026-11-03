# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

# shellcheck shell=bash
# Shared by run.sh and rerun.sh. Sourced, not executed.

# Absolute output directory for <name>, whatever the caller's directory.
out_dir() {
  echo "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/out/$1"
}

# Before #307 the pixel under test appended `.pixel/` to a tracked
# .gitignore that lacked it. Called only right after that pixel command ran
# on a tree require_clean accepted, so the edit is the benchmark's own; it
# undoes exactly a .gitignore whose only change is one added `.pixel/` line.
drop_pixel_gitignore_edit() {
  [ "$(git diff --numstat -- .gitignore)" = "$(printf '1\t0\t.gitignore')" ] || return 0
  if git diff -U0 -- .gitignore | grep -qx '+\.pixel/'; then
    git checkout -q -- .gitignore
  fi
}

# Refuse a tree with tracked edits: they would ride along into every
# measured diff, and a checkout could lose them. Nothing is undone here.
require_clean() {
  if [ -n "$(git status --porcelain --untracked-files=no)" ]; then
    echo "refusing: $(pwd) has tracked edits" >&2
    git status --short --untracked-files=no >&2
    return 1
  fi
}

# Where the tree started: its branch when on one, else the commit.
start_state() {
  git symbolic-ref -q --short HEAD || git rev-parse HEAD
}

# Measure one commit: check it out, then map its diff onto the refreshed
# graph. Appends one line to progress.txt, or to <failures> on a missing
# parent or a failed checkout. Usage: measure <out> <failures> <index> <commit> [tag]
measure() {
  local out=$1 failures=$2 i=$3 c=$4 tag=${5:-}
  if ! git cat-file -e "$c^" 2>/dev/null; then
    # A shallow clone's boundary: no base, so no diff to measure.
    echo "$i $c no-parent" >> "$failures"
    return
  fi
  # --no-overwrite-ignore: an ignored file on a path the commit tracks
  # fails the checkout instead of being replaced.
  if ! git checkout -q --no-overwrite-ignore --detach "$c"; then
    echo "$i $c checkout-failed" >> "$failures"
    return
  fi
  local f t0 t1 rc
  f="$out/$(printf %03d "$i")-$c"
  t0=$(perl -MTime::HiRes=time -e 'printf "%.3f", time')
  PIXEL_METRICS=0 "$BIN" what-changed --base "$c^" --json . > "$f.json" 2> "$f.err"
  rc=$?
  t1=$(perl -MTime::HiRes=time -e 'printf "%.3f", time')
  drop_pixel_gitignore_edit
  echo "$i $c rc=$rc secs=$(echo "$t1 - $t0" | bc)${tag:+ $tag}" >> "$out/progress.txt"
}

# Put the tree back where start_state found it (the branch itself, not a
# detached copy of its commit) and say so if that failed. Meant for an EXIT
# trap, so an interrupted run is restored too.
restore() {
  local state=$1
  drop_pixel_gitignore_edit
  if git show-ref -q --verify "refs/heads/$state"; then
    git checkout -q --no-overwrite-ignore "$state" &&
      [ "$(git symbolic-ref -q --short HEAD)" = "$state" ] && return 0
  else
    git checkout -q --no-overwrite-ignore --detach "$state" &&
      [ "$(git rev-parse HEAD)" = "$(git rev-parse "$state")" ] && return 0
  fi
  echo "warning: $(pwd) was not restored to $state" >&2
  return 1
}
