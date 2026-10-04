#!/usr/bin/env bash
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

# Codex runner for the eval loop. Reads $WT $CFG $PROMPT $OUT; writes codex
# exec --json (JSONL events) to $OUT. Arm isolation: $CFG/codex-home holds a
# per-arm config.toml (baseline strips the pixel block, variants swap it,
# quiet/full keep the installed one) plus a copy of auth.json and, for
# quiet/full, the arm's hooks.json, selected via CODEX_HOME.
#
# Pinned per campaign: CODEX_MODEL (`--model`) and CODEX_REASONING
# (`-c model_reasoning_effort=`). CODEX_SANDBOX picks the sandbox:
#   bypass (default)  --dangerously-bypass-approvals-and-sandbox, the same
#                     authority Claude's --dangerously-skip-permissions run
#                     gets: edit tasks build and test, and pixel can write
#                     .pixel/ and its daemon socket in every arm
#   read-only         the previous default; it blocks pixel's own writes
# --dangerously-bypass-hook-trust: the per-arm hooks.json lives at a scratch
# path no `/hooks` review ever trusted; without it the quiet/full hooks
# would silently not run. The arm builder is what vets them.
set -euo pipefail
: "${WT:?}" "${CFG:?}" "${PROMPT:?}" "${OUT:?}" "${CODEX_MODEL:?}"
export CODEX_HOME="$CFG/codex-home"
sandbox=(--dangerously-bypass-approvals-and-sandbox)
case "${CODEX_SANDBOX:-bypass}" in
  bypass) ;;
  read-only|workspace-write) sandbox=(--sandbox "$CODEX_SANDBOX") ;;
  *) echo "CODEX_SANDBOX must be bypass, read-only or workspace-write" >&2; exit 2 ;;
esac
reasoning=()
[ -n "${CODEX_REASONING:-}" ] && reasoning=(-c "model_reasoning_effort=\"$CODEX_REASONING\"")
cd "$WT"
"${CODEX_BIN:-$HOME/.local/bin/codex}" exec \
  --json --ephemeral --skip-git-repo-check --dangerously-bypass-hook-trust \
  "${sandbox[@]}" --model "$CODEX_MODEL" ${reasoning[@]+"${reasoning[@]}"} -C "$WT" \
  "$PROMPT" > "$OUT" 2> "${OUT%.jsonl}.err"
