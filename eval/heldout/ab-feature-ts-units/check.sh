#!/bin/sh
# Held-out check for ab-feature-ts-units. Runs in the agent's worktree after
# the agent exits: a test target the agent never saw, the crate's own unit
# tests, and the two user-facing strings that list the units.
set -u
fail=0
# The --ts help text and the bad-duration message each list the units on
# one line; after the change both lines must show a week and a millisecond
# example (wording is free).
lines=$(grep -E '[0-9]+w([^A-Za-z0-9_]|$)' crates/pixel/src/sniper_cmd.rs | grep -Ec '[0-9]+ms([^A-Za-z0-9_]|$)' || true)
if [ "${lines:-0}" -lt 2 ]; then
  echo "FAIL: crates/pixel/src/sniper_cmd.rs has ${lines:-0} line(s) listing both a week (2w) and a millisecond (250ms) example; the help text and the error message need one each"
  fail=1
fi
[ "$fail" = 0 ] || exit 1
cp "$HELDOUT/heldout_ts_units.rs" crates/pixel-session/tests/heldout_ts_units.rs
cargo test -q -p pixel-session --test heldout_ts_units || { echo "FAIL: held-out test"; exit 1; }
cargo test -q -p pixel-session --lib || { echo "FAIL: the crate's own unit tests"; exit 1; }
echo PASS
