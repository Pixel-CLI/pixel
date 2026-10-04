#!/bin/sh
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

# Held-out check for ab-rename-graph-db-file. Runs in the agent's worktree
# after the agent exits ($HELDOUT = this directory's copy).
# Passes when: no whole-word GRAPH_DB_FILE is left in any tracked file
# (comments and Markdown included), every file that held it now names
# GRAPH_DB_FILENAME, the value is unchanged, and the three crates that use
# it still compile with their tests.
set -u
fail=0
if git grep -n -w GRAPH_DB_FILE -- . ':!eval'; then
  echo "FAIL: GRAPH_DB_FILE is still spelled above"
  fail=1
fi
while read -r file; do
  [ -n "$file" ] || continue
  if ! grep -q -w GRAPH_DB_FILENAME "$file"; then
    echo "FAIL: $file does not name GRAPH_DB_FILENAME"
    fail=1
  fi
done < "$HELDOUT/files.txt"
if ! grep -q 'pub const GRAPH_DB_FILENAME: &str = "graph.v2.db";' crates/pixel-daemon/src/api.rs; then
  echo "FAIL: crates/pixel-daemon/src/api.rs no longer defines GRAPH_DB_FILENAME = \"graph.v2.db\""
  fail=1
fi
[ "$fail" = 0 ] || exit 1
cargo check -q -p pixel-daemon -p pixel-install -p pixel-cli --all-targets || { echo "FAIL: cargo check"; exit 1; }
echo PASS
