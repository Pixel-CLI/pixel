#!/bin/sh
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

# Pre-push mutants gate that runs the campaign off the laptop. The committed
# three-dot diff is bundled to the gate host (default: ssh alias `a2`), which
# checks it out, seeds the shared mutant-outcome cache and executes the same
# `mutants-preflight.sh --run` campaign CI shards run, against a warm target/.
# The push is blocked on the remote verdict; exit codes are CI's (0 caught,
# 2 missed, 3 timeout, 4 baseline). `PIXEL_MUTANTS_GATE=off` skips the remote
# run; `git push --no-verify` remains Git's explicit local bypass.
set -eu

repo=$(git rev-parse --show-toplevel 2>/dev/null) \
    || { echo "mutation gate: not inside a Git repository" >&2; exit 2; }
cd "$repo"

if [ "${PIXEL_MUTANTS_GATE:-on}" = off ]; then
    echo "mutation gate: off (PIXEL_MUTANTS_GATE=off); CI's Mutants in diff stays the merge gate"
    exit 0
fi

base=${PIXEL_MUTANTS_BASE:-origin/main}
base_oid=$(git rev-parse --verify "$base^{commit}" 2>/dev/null) \
    || { echo "mutation gate: base '$base' is unavailable; fetch it or set PIXEL_MUTANTS_BASE=<commit-or-ref>" >&2; exit 2; }
head_oid=$(git rev-parse --verify HEAD^{commit})
changed=$(git diff --name-only "$base_oid...$head_oid") \
    || { echo "mutation gate: could not calculate '$base...HEAD'" >&2; exit 2; }

if ! printf '%s\n' "$changed" | grep -Eq '^crates/.*\.rs$'; then
    echo "mutation gate: not applicable; no mutable Rust changed"
    exit 0
fi

host=${PIXEL_MUTANTS_GATE_HOST:-a2}
remote_script=${PIXEL_MUTANTS_GATE_SCRIPT:-pixel-mutants-gate}

tmp_dir=$(mktemp -d "${TMPDIR:-/tmp}/pixel-mutants-gate.XXXXXX") \
    || { echo "mutation gate: could not create temporary files" >&2; exit 2; }
trap 'rm -rf "$tmp_dir"' EXIT HUP INT TERM
bundle_file="$tmp_dir/push.bundle"

# The branch is unpushed by definition here, so the commits travel as a bundle
# with `base..HEAD` prerequisites the host satisfies from its own origin fetch.
# The positive side must be the named ref HEAD: a bundle refuses to record a
# bare object id.
if ! git bundle create "$bundle_file" "$base_oid..HEAD" >&2; then
    echo "mutation gate: could not bundle $base_oid..HEAD" >&2
    exit 2
fi

# One traveling outcome cache: a local `--run` may have caught mutants from
# this branch's earlier heads already; seed the host with them so the remote
# campaign re-tests only the rest, and adopt the host's refreshed outcomes
# afterwards so a later local run starts from them. Outcomes are keyed by
# source span and replacement, so a stale seed only costs re-tests.
seed_dir="$repo/target/mutants-preflight"
payload="$tmp_dir/gate-payload.tar"
COPYFILE_DISABLE=1 tar -cf "$payload" -C "$tmp_dir" push.bundle
if [ -f "$seed_dir/mutants.out" ]; then
    COPYFILE_DISABLE=1 tar -rf "$payload" -C "$seed_dir" mutants.out
fi

echo "mutation gate: running the diff's mutants on $host (warm cache) ..."
status=0
ssh -o BatchMode=yes -o ConnectTimeout=10 "$host" \
    "$remote_script" "$base_oid" "$head_oid" < "$payload" || status=$?
case $status in
    0) ;;
    2) echo "mutation gate: MISSED mutants on $host; fix them before pushing" >&2 ;;
    3) echo "mutation gate: timed-out mutants on $host; fix them before pushing" >&2 ;;
    4) echo "mutation gate: baseline tests fail on $host" >&2 ;;
    255) echo "mutation gate: cannot reach $host (it must be up, with \`$remote_script\` on its PATH)" >&2 ;;
    *) echo "mutation gate: remote run exited $status" >&2 ;;
esac

# Adopt the host's refreshed outcome cache either way: even a blocked push
# leaves behind which mutants already failed, and the next attempt re-tests
# only what is left.
if [ "$status" -le 4 ]; then
    host_out=${PIXEL_MUTANTS_GATE_OUT:-workers/pixel/target/mutants-preflight/mutants.out}
    if ssh -o BatchMode=yes -o ConnectTimeout=10 "$host" \
            "cat \$HOME/$host_out 2>/dev/null" > "$tmp_dir/host.out" \
            && [ -s "$tmp_dir/host.out" ]; then
        mkdir -p "$seed_dir"
        cp "$tmp_dir/host.out" "$seed_dir/mutants.out"
    fi
fi
exit "$status"
