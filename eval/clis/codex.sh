#!/usr/bin/env bash
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

# Codex runner for the eval loop. Reads $WT $CFG $PROMPT $OUT; writes codex
# exec --json (JSONL events) to $OUT. Arm isolation: $CFG/codex-home holds a
# per-arm config.toml (baseline strips the pixel block, variants swap it) plus
# a copy of auth.json, selected via CODEX_HOME.
set -euo pipefail
: "${WT:?}" "${CFG:?}" "${PROMPT:?}" "${OUT:?}"
export CODEX_HOME="$CFG/codex-home"
cd "$WT"
"${CODEX_BIN:-$HOME/.local/bin/codex}" exec \
  --json --sandbox read-only --skip-git-repo-check \
  "$PROMPT" > "$OUT" 2> "${OUT%.jsonl}.err"
