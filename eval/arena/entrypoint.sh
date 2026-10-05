#!/usr/bin/env bash
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

# Arena entrypoint: per-arm repo preparation (indexing + codex wiring), then
# the scored codex run. $ARM_TOOL selects the arm; $PROMPT/$OUT are the task.
set -uo pipefail
export PATH="$HOME/.local/bin:$PATH"
REPO_DIR="${ARENA_REPO_DIR:-/repo}"
cd "$REPO_DIR" || exit 1

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
prep_pixel() {
  local prep_start_ms prep_end_ms graph_db setup_receipt
  if [ "${PIXEL_ARENA_PREP_GRAPH:-0}" = "1" ]; then
    prep_start_ms=$(date +%s%3N) || return $?
  fi
  pixel install || return $?
  if [ "${PIXEL_ARENA_PREP_GRAPH:-0}" = "1" ]; then
    graph_db="$REPO_DIR/.pixel/graph.v2.db"
    setup_receipt="/out/setup-pixel-${REP:-1}.json"
    pixel prepare-repo --no-daemon "$REPO_DIR" || return $?
    if [ ! -f "$graph_db" ]; then
      echo "graph preparation completed without $graph_db" >&2
      return 1
    fi
    prep_end_ms=$(date +%s%3N) || return $?
    python3 -c 'import json,sys; graph=sys.argv[1]; json.dump({"arm":"pixel","rep":sys.argv[4],"prepare_commands":["pixel install","pixel prepare-repo --no-daemon /repo"],"duration_ms":int(sys.argv[2])-int(sys.argv[3]),"graph_db":graph,"graph_db_bytes":__import__("pathlib").Path(graph).stat().st_size},open(sys.argv[5],"w"),indent=2); open(sys.argv[5],"a").write("\n")' \
      "$graph_db" "$prep_end_ms" "$prep_start_ms" "${REP:-1}" "$setup_receipt"
  else
    pixel build-index --history "$REPO_DIR"
  fi
}

case "${ARM_TOOL:-raw}" in
  raw)      : ;;
  semble)   prep_semble ;;
  graft)    prep_graft ;;
  stacklit) prep_stacklit ;;
  gitnexus) prep_gitnexus ;;
  gortex)   prep_gortex ;;
  pixel)    prep_pixel ;;
esac
prep_rc=$?
if [ "$prep_rc" -ne 0 ]; then
  echo "arm preparation failed: arm=${ARM_TOOL:-raw} rc=$prep_rc" >&2
  exit "$prep_rc"
fi

# This receipt is captured after arm setup, immediately before Codex runs, so
# the arena can verify the static instructions visible to each arm.
python3 /usr/local/lib/arena-context-manifest.py \
  --repo "$REPO_DIR" --codex-home "${CODEX_HOME:-$HOME/.codex}" \
  > "/out/context-${ARM_TOOL:-raw}-${REP:-1}.json"

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

overall_rc=0
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
    overall_rc=1
  fi
done
exit "$overall_rc"
