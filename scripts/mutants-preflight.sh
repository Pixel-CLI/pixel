#!/bin/sh
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

# Fast pre-push mutation exposure gate. --check/--ack only list CI's
# prospective mutants; CI remains responsible for the verdict. --run executes
# them locally in a throwaway worktree (optionally bounded by a -F-style
# regex over function names) so a fix can be verified before pushing.
set -eu

mode=${1:---check}
filter=
case "$mode" in
    --check|--ack) ;;
    --run) filter=${2:-} ;;
    *) echo "usage: scripts/mutants-preflight.sh [--check|--ack|--run [regex]]" >&2; exit 2 ;;
esac

repo=$(git rev-parse --show-toplevel 2>/dev/null) \
    || { echo "mutation exposure: not inside a Git repository" >&2; exit 2; }
cd "$repo"

if ! git diff --quiet -- crates || ! git diff --cached --quiet -- crates; then
    echo "mutation exposure: commit or stash tracked Rust changes before listing the committed push diff" >&2
    exit 2
fi

base=${PIXEL_MUTANTS_BASE:-origin/main}
base_oid=$(git rev-parse --verify "$base^{commit}" 2>/dev/null) \
    || { echo "mutation exposure: base '$base' is unavailable; fetch it or set PIXEL_MUTANTS_BASE=<commit-or-ref>" >&2; exit 2; }
head_oid=$(git rev-parse --verify HEAD^{commit})
changed=$(git diff --name-only "$base_oid...$head_oid") \
    || { echo "mutation exposure: could not calculate '$base...HEAD'" >&2; exit 2; }

if ! printf '%s\n' "$changed" | grep -Eq '^crates/.*\.rs$'; then
    echo "mutation exposure: not applicable; no mutable Rust changed"
    exit 0
fi

command -v cargo >/dev/null 2>&1 \
    || { echo "mutation exposure: cargo is required to list mutants" >&2; exit 2; }

tmp_dir=$(mktemp -d "${TMPDIR:-/tmp}/pixel-mutants-preflight.XXXXXX") \
    || { echo "mutation exposure: could not create temporary files" >&2; exit 2; }
trap 'rm -rf "$tmp_dir"' EXIT HUP INT TERM
diff_file="$tmp_dir/pr.diff"
listing_file="$tmp_dir/listing.txt"
stderr_file="$tmp_dir/stderr.txt"

git diff "$base_oid...$head_oid" > "$diff_file"
if ! cargo mutants --list --in-diff "$diff_file" > "$listing_file" 2> "$stderr_file"; then
    echo "mutation exposure: cargo mutants --list failed" >&2
    sed -n '1,120p' "$stderr_file" >&2
    exit 2
fi

# cargo-mutants emits one source-location line per prospective mutation.
count=$(grep -Ec '^.*\.rs:[0-9]+:[0-9]+:' "$listing_file" || true)
if [ "$count" -eq 0 ]; then
    echo "mutation exposure: vacuous; mutable Rust changed but cargo-mutants listed no mutations"
else
    echo "mutation exposure: $count prospective mutant(s) for $base...HEAD"
    sed -n '1,240p' "$listing_file"
    if [ "$(wc -l < "$listing_file" | tr -d ' ')" -gt 240 ]; then
        echo "mutation exposure: listing truncated after 240 lines; rerun scripts/mutants-preflight.sh --ack to review the exact receipt"
    fi
fi

hash_file() {
    if command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{print $1}'
    else
        sha256sum "$1" | awk '{print $1}'
    fi
}

receipt=$(git rev-parse --git-path pixel-mutants-preflight-receipt)
fingerprint="head=$head_oid
base=$base_oid
diff=$(hash_file "$diff_file")
listing=$(hash_file "$listing_file")
count=$count"

if [ "$mode" = --ack ]; then
    printf '%s\n' "$fingerprint" > "$receipt"
    echo "mutation exposure: receipt recorded for $count prospective mutant(s)"
    exit 0
fi

if [ "$mode" = --run ]; then
    # The CI shard's command without `--shard`: the cargo arguments come
    # from .cargo/mutants.toml for both, and so must the cargo-mutants version.
    sh "$repo/scripts/mutants-version-check.sh" "$repo"
    worktree="$tmp_dir/tree"
    git worktree add --detach "$worktree" "$head_oid" >/dev/null
    # Iterate, unfiltered runs only: the previous run's outcomes are kept
    # under target/ and copied into the throwaway tree, so a rerun after
    # fixing a MISSED line re-tests only the mutants not yet caught (and any
    # whose source moved) instead of the whole list. A filtered run judges a
    # different slice, so its outcomes must never seed or replace the full
    # run's. First run has no seed and --iterate is a no-op.
    iterate=
    persist="$repo/target/mutants-preflight"
    if [ -z "$filter" ]; then
        iterate=--iterate
        mkdir -p "$persist"
        if [ -d "$persist/mutants.out" ]; then
            cp -R "$persist/mutants.out" "$worktree/mutants.out"
        fi
    fi
    run_status=0
    (
        cd "$worktree" || exit 2
        if [ -n "$filter" ]; then
            exec cargo mutants -vV --no-shuffle --in-place --in-diff "$diff_file" \
                -F "$filter"
        else
            exec cargo mutants -vV --no-shuffle --in-place --iterate --in-diff "$diff_file"
        fi
    ) || run_status=$?
    if [ -n "$iterate" ]; then
        if [ -d "$worktree/mutants.out" ]; then
            rm -rf "$persist/mutants.out"
            cp -R "$worktree/mutants.out" "$persist/mutants.out"
        fi
    fi
    outcomes="$worktree/mutants.out/mutants.out"
    if [ -f "$outcomes" ]; then
        survivors=$(grep -Ei '^(missed|timeout)' "$outcomes" || true)
        if [ -n "$survivors" ]; then
            echo "mutation exposure: surviving mutant(s) in this run:" >&2
            printf '%s\n' "$survivors" >&2
        fi
    fi
    git worktree remove --force "$worktree" 2>/dev/null || true
    case "$run_status" in
        0) echo "mutation exposure: local run caught every listed mutant" ;;
        2) echo "mutation exposure: local run has MISSED mutants; fix before pushing" >&2 ;;
        3) echo "mutation exposure: local run timed out mutants; fix before pushing" >&2 ;;
        4) echo "mutation exposure: baseline tests already fail; fix before pushing" >&2 ;;
        *) echo "mutation exposure: cargo mutants exited $run_status" >&2 ;;
    esac
    exit "$run_status"
fi

if [ -f "$receipt" ] && [ "$(cat "$receipt")" = "$fingerprint" ]; then
    echo "mutation exposure: reviewed receipt matches this push"
    exit 0
fi

echo "mutation exposure: push blocked until this exact listing is reviewed" >&2
echo "For every listed mutant, name the test that fails or add it before pushing." >&2
echo "Review the listing, then run: scripts/mutants-preflight.sh --ack" >&2
exit 1
