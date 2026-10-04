#!/bin/sh
# Held-out check for ab-bugfix-global-excludes (pinned to 4aa6790, the parent
# of the historical fix 2732fa2). Runs in the agent's worktree after the
# agent exits: adds a test target the agent never saw, then runs it and the
# crate's own unit tests.
set -u
mkdir -p crates/pixel-index/tests   # the crate has no tests/ directory at 4aa6790
cp "$HELDOUT/heldout_global_excludes.rs" crates/pixel-index/tests/heldout_global_excludes.rs
cargo test -q -p pixel-index --test heldout_global_excludes || { echo "FAIL: held-out test"; exit 1; }
cargo test -q -p pixel-index --lib || { echo "FAIL: the crate's own unit tests"; exit 1; }
echo PASS
