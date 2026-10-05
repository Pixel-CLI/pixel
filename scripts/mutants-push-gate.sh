#!/bin/sh
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

# Manual legacy mutation wrapper, no longer called by the pre-push hook.
# Automatic mutation feedback runs nightly on main. `PIXEL_MUTANTS_GATE=local`
# runs the campaign CI's shards run -- `scripts/mutants-preflight.sh --run`,
# on the nightly and cargo-mutants the workflow pins -- on the pushing machine,
# against PIXEL_MUTANTS_BASE (origin/main by default), returning the
# campaign's exit code. This compatibility entry point is manual only;
# it is not part of the agent or pre-push loop.
set -eu

repo=$(git rev-parse --show-toplevel 2>/dev/null) \
    || { echo "mutation gate: not inside a Git repository" >&2; exit 2; }

mode=${PIXEL_MUTANTS_GATE:-off}
case "$mode" in
    off)
        echo "mutation gate: off (PIXEL_MUTANTS_GATE=local runs a manual diff campaign); automatic mutations run nightly on main"
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
