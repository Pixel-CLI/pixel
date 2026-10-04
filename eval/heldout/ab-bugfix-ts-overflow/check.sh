#!/bin/sh
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

# Held-out check for ab-bugfix-ts-overflow (pinned to e99939b, the parent of
# the historical fix 313bb53). Runs in the agent's worktree after the agent
# exits: adds a test target the agent never saw, then runs it and the
# crate's own unit tests.
set -u
cp "$HELDOUT/heldout_ts_window.rs" crates/pixel-session/tests/heldout_ts_window.rs
cargo test -q -p pixel-session --test heldout_ts_window || { echo "FAIL: held-out test"; exit 1; }
cargo test -q -p pixel-session --lib || { echo "FAIL: the crate's own unit tests"; exit 1; }
echo PASS
