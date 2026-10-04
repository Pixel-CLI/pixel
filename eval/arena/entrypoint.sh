#!/usr/bin/env bash
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

# Arena entrypoint: per-arm repo preparation (indexing + codex wiring), then
# the scored codex run. $ARM_TOOL selects the arm; $PROMPT/$OUT are the task.
set -uo pipefail
export PATH="$HOME/.local/bin:$PATH"
cd /repo

prep_semble()  { :; }                     # indexes lazily on first query
prep_graft()   { graft init --yes; }
prep_stacklit() {
  npx -y stacklit init
  npx -y stacklit generate
  npx -y stacklit derive > /root/.codex/AGENTS.md
}
prep_gitnexus() { gitnexus setup && gitnexus analyze /repo; }
prep_gortex()  {
  gortex daemon start
  gortex track /repo
  # the daemon must acknowledge the repo before the MCP server is useful;
  # a blind sleep hung the first round
  for _ in $(seq 1 24); do
    gortex repos 2>/dev/null | grep -q "/repo" && break
    sleep 5
  done
  codex mcp add gortex -- gortex mcp
}
prep_pixel()   { pixel install && pixel build-index --history /repo; }

case "${ARM_TOOL:-raw}" in
  raw)      : ;;
  semble)   prep_semble ;;
  graft)    prep_graft ;;
  stacklit) prep_stacklit ;;
  gitnexus) prep_gitnexus ;;
  gortex)   prep_gortex ;;
  pixel)    prep_pixel ;;
esac

# MCP arms: register the tool's stdio server with codex
case "${ARM_TOOL:-raw}" in
  semble)   codex mcp add semble -- uvx --from 'semble[mcp]' semble ;;
  graft)    codex mcp add graft -- graft mcp ;;
  stacklit) codex mcp add stacklit -- npx -y stacklit serve ;;
  gitnexus) : ;;   # gitnexus setup wrote its own MCP entries
  gortex)   : ;;   # registered in prep_gortex
  pixel)    : ;;   # pixel wires itself via pixel install
esac

IFS=' ' read -r -a TASK_LIST <<< "${TASKS:-s1-hook-install s2-vector-recall s3-rename-impact}"
MODEL_ARGS=()
[ -n "${CODEX_MODEL:-}" ] && MODEL_ARGS+=(-m "$CODEX_MODEL")
[ -n "${CODEX_EFFORT:-}" ] && MODEL_ARGS+=(-c model_reasoning_effort="$CODEX_EFFORT")

for task in "${TASK_LIST[@]}"; do
  prompt=$(python3 -c "import json,sys;print(json.load(open(sys.argv[1]))['prompt'])" "/prompts/$task.json")
  tag="${ARM_TOOL}-${task}-${REP}"
  t0=$(date +%s)
  # The container is the sandbox: docker's default seccomp blocks codex's
  # bubblewrap namespaces, so read-only mode would fail every command.
  codex exec --json --sandbox danger-full-access --skip-git-repo-check \
    "${MODEL_ARGS[@]}" "$prompt" > "/out/$tag.jsonl" 2> /tmp/err
  rc=$?
  t1=$(date +%s)
  echo $((t1 - t0)) > "/out/$tag.seconds"
  if [ "$rc" -ne 0 ]; then
    echo "arm=$ARM_TOOL task=$task rc=$rc" >&2
    touch "/out/$tag.failed"
  fi
done
