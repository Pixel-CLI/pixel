#!/bin/sh
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT
# usage: build.sh PIXEL V1_CLONE HYBRID_CLONE HOMES
# Builds both graphs of one corpus with the same binary: v1 with
# PIXEL_GRAPH_SOURCE_RUBY unset, the hybrid with it set to `hybrid`, each
# under its own empty HOME (../ruby-hybrid-daemon-2026-10-18/env.sh).
set -eu
here=$(cd "$(dirname "$0")" && pwd)
env_sh=$here/../ruby-hybrid-daemon-2026-10-18/env.sh
pixel=$1 v1=$2 hybrid=$3 homes=$4
"$pixel" --version | head -2
for arm in v1 hybrid; do
  if [ "$arm" = v1 ]; then repo=$v1; else repo=$hybrid; fi
  git -C "$repo" rev-parse HEAD
  start=$(date +%s)
  if [ "$arm" = v1 ]; then
    env -u PIXEL_GRAPH_SOURCE_RUBY sh "$env_sh" "$homes/$arm" "$pixel" rebuild-graph --metrics off "$repo" >"$homes/$arm.out" 2>&1
  else
    PIXEL_GRAPH_SOURCE_RUBY=hybrid sh "$env_sh" "$homes/$arm" "$pixel" rebuild-graph --metrics off "$repo" >"$homes/$arm.out" 2>&1
  fi
  echo "$arm: rebuild-graph exit 0 in $(($(date +%s) - start)) s"
  sqlite3 "$repo/.pixel/graph.v2.db" "SELECT key, value FROM meta WHERE key LIKE '%ruby%'"
done
