#!/bin/sh
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

# Host side of pixel's pre-push mutants gate (scripts/mutants-remote-gate.sh).
# stdin: tar payload holding push.bundle and an optional mutants.out seed.
# usage: pixel-mutants-gate <base-oid> <head-oid>
set -eu
base_oid=$1
head_oid=$2

repo=$HOME/workers/pixel
exec 9>"$repo/.mutants-gate.lock"
flock -w 900 9 || { echo "pixel-mutants-gate: another campaign holds the lock" >&2; exit 2; }

tmp=$(mktemp -d /tmp/pixel-mutants-gate.XXXXXX)
trap 'rm -rf "$tmp"' EXIT HUP INT TERM
tar --warning=no-unknown-keyword -xf - -C "$tmp"

cd "$repo"
. "$HOME/.cargo/env"
export CARGO_INCREMENTAL=1
# The profile CI shards build with, so the shared target/ never churns between
# a gate campaign and a shard: line-tables-only debuginfo is what makes a
# one-crate rebuild cost seconds instead of minutes.
export CARGO_PROFILE_DEV_DEBUG=line-tables-only
# The campaign below runs on the pinned nightly of
# scripts/mutants-toolchain.sh (sourced by mutants-preflight.sh --run, which
# this execs): .cargo/mutants.toml hands the test binary --fail-fast, which
# stable libtest rejects. The first nightly campaign rebuilds the warm
# target/ once (stable and nightly artifacts never share fingerprints);
# every later one is warm again.
# The ssh session has no XDG_RUNTIME_DIR; without it the test process and the
# spawned pixel binaries resolve daemon sockets into different homes and the
# daemon fixtures never meet (2026-10-03, three aborted baselines).
export XDG_RUNTIME_DIR=/var/tmp/pixel-rt
# /tmp is a 24G tmpfs and $HOME is itself a git repository with a built
# .pixel index: scratch under either breaks discover_root-dependent tests.
export TMPDIR=/var/tmp/pixel-mutants-tmp
mkdir -p "$XDG_RUNTIME_DIR" "$TMPDIR"

git fetch --quiet origin main
git fetch --quiet "$tmp/push.bundle"
git checkout --quiet --detach "$head_oid"

# Seed the traveling outcome cache; --run's --iterate skips what it already
# caught and re-tests only the rest.
persist="$repo/target/mutants-preflight"
mkdir -p "$persist"
if [ -f "$tmp/mutants.out" ]; then
    cp "$tmp/mutants.out" "$persist/mutants.out"
fi

# The client bundled the diff from this exact base. `origin/main` can advance
# while the payload is in flight, so letting preflight use its default would
# silently validate a different mutation surface than the one being pushed.
exec env PIXEL_MUTANTS_BASE="$base_oid" sh "$repo/scripts/mutants-preflight.sh" --run
