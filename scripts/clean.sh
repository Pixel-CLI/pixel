#!/bin/sh
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

# Reclaim the disk a pixel checkout spends on what it can rebuild.
#
#   scripts/clean.sh                 # build: target/ + tree scratch, every worktree
#   scripts/clean.sh --dry-run       # print the plan and its total, remove nothing
#   scripts/clean.sh index           # .pixel/ of every worktree (daemon stopped first)
#   scripts/clean.sh cache bench     # shared shard cache, /tmp bench scratch
#   scripts/clean.sh all             # every scope above
#
# Scopes, each named by what it costs to get the bytes back:
#
#   build  target/ of every worktree of this repository (`git worktree list`,
#          not only the one this runs from: a laptop carrying four worktrees
#          carries four workspace builds -- which also means a build, a test
#          run or a mutants campaign running in another worktree loses its
#          output mid-flight), plus the ignored scratch a run
#          leaves in the tree -- mutants.out*, __pycache__, node_modules,
#          coverage dumps, and the files the competitor tools write during
#          scripts/bench-vs (.gitnexus/, stacklit.*, DEPENDENCIES.md).
#          Cost: one cargo build, one bun install.
#   index  .pixel/ of every worktree: base and delta shards, graph.db,
#          history.db, the search-meaning vector cache (code-vectors/), and
#          the local action log with them. Cost: `pixel build-index
#          --history .`, minutes on a repository of a few hundred commits,
#          plus one full embedding pass on the next `pixel search-meaning`.
#          The daemon serves that index and writes to it, so each root's
#          daemon is stopped first.
#   cache  the base-shard cache shared by every worktree
#          (`$XDG_CACHE_HOME`, else ~/.cache)/pixel/shards. It is
#          content-addressed and refilled by the next index build, but it is
#          also what makes a second worktree at the same commit cheap, so
#          clearing it bills every worktree for its next build. The daemon
#          sockets that live next to it are never touched.
#   bench  /tmp/pixel-bench-* scratch from scripts/pixel-bench.sh.
#
# Never touched, because this tree cannot rebuild it: the recall corpus
# (~/.local/share/pixel/recall) and the operation state under
# ~/.local/state/pixel (publish-recovery, journals). Remove those by hand if
# you mean to lose them.
#
# Two rules are what make an `rm -rf` built from a list of names safe, and
# scripts/test-clean.py pins both: inside a worktree nothing is removed unless
# git ignores it, and outside one nothing is removed unless it is the pixel
# shard cache or matches the /tmp bench prefix.
set -eu

DRY=0
SCOPES=""
for arg in "$@"; do
    case "$arg" in
        -n|--dry-run) DRY=1 ;;
        build|index|cache|bench) SCOPES="$SCOPES $arg" ;;
        all) SCOPES="$SCOPES build index cache bench" ;;
        # The header above, to the first line that is not a comment. A line
        # range would have to be moved by every edit to it, and the edit that
        # forgets truncates this help mid-sentence without failing anything.
        -h|--help) sed -n '2,/^[^#]/p' "$0" | sed -n 's/^# \{0,1\}//p'; exit 0 ;;
        *) echo "clean.sh: unknown argument: $arg" >&2; exit 2 ;;
    esac
done
[ -n "$SCOPES" ] || SCOPES=" build"

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

# Every worktree of this repository, main checkout included. Fail open: when
# git cannot answer, this checkout is the only root.
WORKTREES="$(git worktree list --porcelain 2>/dev/null | sed -n 's/^worktree //p' || true)"
[ -n "$WORKTREES" ] || WORKTREES="$ROOT"

CACHE_BASE="${XDG_CACHE_HOME:-${HOME:-/nonexistent}/.cache}"

# cargo honours CARGO_TARGET_DIR over <worktree>/target and points every
# worktree at that one directory. A `build.target-dir` in a cargo config file
# is not read here (this repository sets none); with one, clean the directory
# it names by hand. Canonicalised so it is recognised as a root of its own.
TARGET_OVERRIDE=""
if [ -n "${CARGO_TARGET_DIR:-}" ]; then
    TARGET_OVERRIDE="$(cd "$CARGO_TARGET_DIR" 2>/dev/null && pwd -P || true)"
fi

PIXEL="${PIXEL_BIN:-$(command -v pixel 2>/dev/null || true)}"
if [ -z "$PIXEL" ]; then
    for p in "$ROOT/target/dev-release/pixel" "$ROOT/target/release/pixel"; do
        if [ -x "$p" ]; then
            PIXEL="$p"
            break
        fi
    done
fi

TAB="$(printf '\t')"
PLAN="$(mktemp)"
# A `find` pipeline plans in a subshell, so a refusal recorded in a shell
# variable there would be lost by the time the exit code is decided. The
# marker is a file for the same reason the plan is.
REFUSALS="$(mktemp)"
trap 'rm -f "$PLAN" "$REFUSALS"' EXIT INT TERM

# owner <path> -- the worktree the path sits in, empty when none. Worktrees
# nest here (.claude/worktrees/* live inside the main checkout), so the
# longest match wins, not the first.
owner() {
    _p="$1"; _found=""
    while IFS= read -r _r; do
        [ -n "$_r" ] || continue
        case "$_p" in
            "$_r"/*)
                if [ "${#_r}" -gt "${#_found}" ]; then
                    _found="$_r"
                fi
                ;;
        esac
    done <<EOF
$WORKTREES
EOF
    printf '%s' "$_found"
}

# refuse <path> <reason> -- a candidate this script built but must not remove.
# That is a bug in the collection above rather than a state of the tree, so it
# is loud and turns the run red instead of passing quietly.
refuse() {
    printf 'clean.sh: refusing %s: %s\n' "$1" "$2" >&2
    printf '%s\n' "$1" >> "$REFUSALS"
}

# fail <path> -- a planned removal that did not happen (permissions, a file
# held open). Same red exit as a refusal, different cause.
fail() {
    printf 'clean.sh: could not remove %s\n' "$1" >&2
    printf '%s\n' "$1" >> "$REFUSALS"
}

# plan <path> -- queue a path for removal, after the checks that make an
# unrecoverable `rm -rf` safe.
plan() {
    p="$1"
    case "$p" in /*) ;; *) return 0 ;; esac
    [ -e "$p" ] || return 0
    # A symlink is never removed: `rm -rf` on the link leaves the bytes it
    # points at, and following it would delete a tree this script never chose.
    if [ -L "$p" ]; then
        printf 'clean.sh: %s is a symlink, skipped\n' "$p" >&2
        return 0
    fi
    wt="$(owner "$p")"
    if [ -n "$wt" ]; then
        # Inside a worktree, git's ignore rules are the authority on what is
        # disposable: a file git tracks stays, whatever pattern its name
        # matched above.
        if ! git -C "$wt" check-ignore -q "$p" 2>/dev/null; then
            refuse "$p" "git does not ignore it"
            return 0
        fi
    else
        case "$p" in
            "$CACHE_BASE"/pixel/shards) ;;
            /tmp/pixel-bench-*|/private/tmp/pixel-bench-*) ;;
            *)
                if [ -z "$TARGET_OVERRIDE" ] || [ "$p" != "$TARGET_OVERRIDE" ]; then
                    refuse "$p" "outside this repository, the pixel cache and the bench scratch"
                    return 0
                fi
                ;;
        esac
    fi
    kb="$(du -sk "$p" 2>/dev/null | cut -f1)"
    [ -n "$kb" ] || kb=0
    printf '%s%s%s\n' "$kb" "$TAB" "$p" >> "$PLAN"
}

collect_build() {
    [ -z "$TARGET_OVERRIDE" ] || plan "$TARGET_OVERRIDE"
    while IFS= read -r wt; do
        [ -n "$wt" ] || continue
        [ -n "$TARGET_OVERRIDE" ] || plan "$wt/target"
        for m in "$wt"/mutants.out*; do plan "$m"; done
        for c in "$wt"/coverage*.txt; do plan "$c"; done
        for s in "$wt/.gitnexus" "$wt/stacklit.json" "$wt/stacklit.html" \
                 "$wt/DEPENDENCIES.md" "$wt/docs/pixel-line-ascii.ppm"; do
            plan "$s"
        done
        # Prune what another root owns (a nested worktree under .claude) and
        # what is planned whole elsewhere (target, .pixel), then print each
        # match without descending into it.
        find "$wt" \
            \( -name .git -o -name target -o -name .claude -o -name .pixel \) -prune -o \
            \( -name __pycache__ -o -name node_modules \) -type d -print -prune 2>/dev/null \
        | while IFS= read -r d; do plan "$d"; done
        find "$wt" \
            \( -name .git -o -name target -o -name .claude -o -name .pixel \) -prune -o \
            -name '*.lcov' -type f -print 2>/dev/null \
        | while IFS= read -r f; do plan "$f"; done
    done <<EOF
$WORKTREES
EOF
}

collect_index() {
    while IFS= read -r wt; do
        [ -n "$wt" ] || continue
        plan "$wt/.pixel"
    done <<EOF
$WORKTREES
EOF
}

collect_cache() {
    plan "$CACHE_BASE/pixel/shards"
}

collect_bench() {
    for d in /tmp/pixel-bench-*; do plan "$d"; done
}

for scope in $SCOPES; do
    case "$scope" in
        build) collect_build ;;
        index) collect_index ;;
        cache) collect_cache ;;
        bench) collect_bench ;;
    esac
done

human() {
    awk -v kb="$1" 'BEGIN {
        if (kb >= 1048576) printf "%.1fG", kb / 1048576;
        else if (kb >= 1024) printf "%.0fM", kb / 1024;
        else printf "%dK", kb;
    }'
}

verdict() {
    [ ! -s "$REFUSALS" ] || exit 1
    exit 0
}

if [ ! -s "$PLAN" ]; then
    printf 'clean.sh:%s: nothing to reclaim\n' "$SCOPES"
    verdict
fi

sort -rn "$PLAN" | while IFS="$TAB" read -r kb path; do
    printf '  %6s  %s\n' "$(human "$kb")" "$path"
done
TOTAL="$(awk -F"$TAB" '{ s += $1 } END { print s + 0 }' "$PLAN")"

if [ "$DRY" -eq 1 ]; then
    printf 'clean.sh:%s: dry run, %s would be reclaimed, nothing removed\n' \
        "$SCOPES" "$(human "$TOTAL")"
    verdict
fi

while IFS="$TAB" read -r kb path; do
    case "$path" in
        */.pixel)
            # The daemon serves this index and writes to it; removing it from
            # under a live daemon races that. A missing binary, or a daemon
            # that is not running, is not an error.
            if [ -n "$PIXEL" ]; then
                "$PIXEL" daemon stop "$(dirname "$path")" --metrics off >/dev/null 2>&1 || true
            fi
            ;;
    esac
    if ! rm -rf "$path"; then
        fail "$path"
    fi
done < "$PLAN"

printf 'clean.sh:%s: reclaimed %s\n' "$SCOPES" "$(human "$TOTAL")"
case " $SCOPES " in
    *" index "*) printf 'clean.sh: rebuild an index with: pixel build-index --history .\n' ;;
esac
verdict
