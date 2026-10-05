#!/bin/sh
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

# Pre-push mutants gate, opt-in. Unset or `off`, it runs nothing and CI's
# `Mutants in diff` stays the only mutation verdict. `PIXEL_MUTANTS_GATE=local`
# runs the campaign CI's shards run -- `scripts/mutants-preflight.sh --run`,
# on the nightly and cargo-mutants the workflow pins -- on the pushing machine,
# against the base the hook passes in PIXEL_MUTANTS_BASE, and blocks the push
# on its verdict with CI's exit codes (0 caught, 2 missed, 3 timeout,
# 4 baseline). It is meant for a machine whose CPUs nothing else waits on,
# such as a cloud agent session with its own VM and target/: on a laptop a
# campaign holds the CPU for minutes to hours. `git push --no-verify` remains
# Git's explicit local bypass.
set -eu

repo=$(git rev-parse --show-toplevel 2>/dev/null) \
    || { echo "mutation gate: not inside a Git repository" >&2; exit 2; }

mode=${PIXEL_MUTANTS_GATE:-off}
case "$mode" in
    off)
        echo "mutation gate: off (PIXEL_MUTANTS_GATE=local runs the diff's mutants before the push); CI's Mutants in diff stays the merge gate"
        exit 0
        ;;
    local) ;;
    *)
        echo "mutation gate: PIXEL_MUTANTS_GATE='$mode' is neither off nor local" >&2
        exit 2
        ;;
esac

echo "mutation gate: running the diff's mutants on this machine (PIXEL_MUTANTS_GATE=local) ..."
exec sh "$repo/scripts/mutants-preflight.sh" --run
