#!/usr/bin/env bash
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

# Arena entrypoint: per-arm repo preparation (indexing + codex wiring), then
# the scored codex run. $ARM_TOOL selects the arm; $PROMPT/$OUT are the task.
set -uo pipefail
export PATH="$HOME/.local/bin:$PATH"
REPO_DIR="${ARENA_REPO_DIR:-/repo}"
if [ -n "${ARENA_CODEX_CALLER_FACTS:-}" ] && [ "${ARENA_CODEX_CALLER_FACTS}" != "0" ]; then
  echo "ARENA_CODEX_CALLER_FACTS is retired" >&2
  exit 2
fi
cd "$REPO_DIR" || exit 1
REVIEWED_PIXEL_HOOKS="${ARENA_REVIEWED_PIXEL_HOOKS:-0}"
SKILL_PILOT="${ARENA_SKILL_PILOT:-0}"
case "$REVIEWED_PIXEL_HOOKS:$SKILL_PILOT" in
  0:0|1:0|0:1) ;;
  *) echo "skill-only runs cannot use reviewed Pixel hooks" >&2; exit 2 ;;
esac
HOOK_AUDIT_KEY="${ARENA_RUN_ID:-standalone}-${REP:-1}"
HOOK_AUDIT_READY="/out/hook-audit-${ARM_TOOL:-raw}-${HOOK_AUDIT_KEY}.ready"
mark_hook_audit_failure() {
  local rc=$?
  if [ "$REVIEWED_PIXEL_HOOKS" = "1" ] && [ ! -e "$HOOK_AUDIT_READY" ]; then
    touch "/out/hook-audit-${ARM_TOOL:-raw}-${HOOK_AUDIT_KEY}.failed"
  fi
  return "$rc"
}
trap mark_hook_audit_failure EXIT

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
  # the classify skill is the conditional-routing surface Codex sees;
  # non-interactive installs skip the wizard, so deploy it explicitly —
  # without this the pixel arm has pixel installed but undiscoverable
  # optional on older binaries: ignore unknown-subcommand failures
  pixel config install-helpers 2>/dev/null || true
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

prep_skill_pilot() {
  local start end graph_db setup_receipt
  start=$(date +%s%3N) || return $?
  graph_db="$REPO_DIR/.pixel/graph.v2.db"
  pixel prepare-repo --no-daemon "$REPO_DIR" || return $?
  if [ ! -f "$graph_db" ]; then
    echo "skill-pilot preparation completed without $graph_db" >&2
    return 1
  fi
  end=$(date +%s%3N) || return $?
  setup_receipt="/out/setup-${ARM_TOOL:-raw}-${REP:-1}.json"
  python3 -c 'import json,sys; from pathlib import Path; db=Path(sys.argv[1]); json.dump({"arm":sys.argv[4],"rep":sys.argv[5],"prepare_commands":["pixel prepare-repo --no-daemon /repo"],"duration_ms":int(sys.argv[3])-int(sys.argv[2]),"graph_db_bytes":db.stat().st_size},open(sys.argv[6],"w"),indent=2); open(sys.argv[6],"a").write("\n")' \
    "$graph_db" "$start" "$end" "${ARM_TOOL:-raw}" "${REP:-1}" "$setup_receipt"
}

if [ "$SKILL_PILOT" = "1" ]; then
  prep_skill_pilot
  prep_rc=$?
else
case "${ARM_TOOL:-raw}" in
  pixel-chain) prep_pixel ;;
  raw)      : ;;
  semble)   prep_semble ;;
  graft)    prep_graft ;;
  stacklit) prep_stacklit ;;
  gitnexus) prep_gitnexus ;;
  gortex)   prep_gortex ;;
  pixel)    prep_pixel ;;
esac
prep_rc=$?
fi
if [ "$prep_rc" -ne 0 ]; then
  echo "arm preparation failed: arm=${ARM_TOOL:-raw} rc=$prep_rc" >&2
  exit "$prep_rc"
fi

if [ "$REVIEWED_PIXEL_HOOKS" = "1" ]; then
  if ! python3 /usr/local/lib/arena-hook-audit.py audit \
    --arm "${ARM_TOOL:-raw}" --codex-home "${CODEX_HOME:-$HOME/.codex}" \
    --repo "$REPO_DIR" --rep "${REP:-1}" \
    --receipt "/out/hook-audit-${ARM_TOOL:-raw}-${REP:-1}.json"; then
    exit 1
  fi
  touch "$HOOK_AUDIT_READY"
  for _ in $(seq 1 90); do
    if [ -e "/out/hook-audit-raw-${HOOK_AUDIT_KEY}.failed" ] || \
       [ -e "/out/hook-audit-pixel-${HOOK_AUDIT_KEY}.failed" ]; then
      touch "/out/hook-audit-${ARM_TOOL:-raw}-${HOOK_AUDIT_KEY}.failed"
      echo "paired hook audit failed; refusing to start Codex" >&2
      exit 1
    fi
    if [ -e "/out/hook-audit-raw-${HOOK_AUDIT_KEY}.ready" ] && \
       [ -e "/out/hook-audit-pixel-${HOOK_AUDIT_KEY}.ready" ]; then
      break
    fi
    sleep 1
  done
  if [ ! -e "/out/hook-audit-raw-${HOOK_AUDIT_KEY}.ready" ] || \
     [ ! -e "/out/hook-audit-pixel-${HOOK_AUDIT_KEY}.ready" ]; then
    touch "/out/hook-audit-${ARM_TOOL:-raw}-${HOOK_AUDIT_KEY}.failed"
    echo "timed out waiting for both paired hook audits; refusing to start Codex" >&2
    exit 1
  fi
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
HOOK_TRUST_ARGS=()
if [ "$REVIEWED_PIXEL_HOOKS" = "1" ]; then
  # Both paired arms use the same bypass; the allowlist was audited above.
  HOOK_TRUST_ARGS+=(--dangerously-bypass-hook-trust)
fi

overall_rc=0
for task in "${TASK_LIST[@]}"; do
  prompt=$(python3 -c "import json,sys;print(json.load(open(sys.argv[1]))['prompt'])" "/prompts/$task.json")
  if [ "${ARM_TOOL:-raw}" = "pixel-chain" ]; then
    # the evidence chain runs first; the agent gets the compact brief, not
    # raw command output — chain cost lands inside this task's wall time
    brief=$(python3 /usr/local/bin/arena-brief.py "$prompt" 2>/dev/null || true)
    if [ -n "$brief" ]; then
      prompt="$prompt

$brief"
    fi
  fi
  tag="${ARM_TOOL}-${task}-${REP}"
  t0=$(date +%s)
  # The container is the sandbox: docker's default seccomp blocks codex's
  # bubblewrap namespaces, so read-only mode would fail every command.
  codex exec --json --sandbox danger-full-access --skip-git-repo-check \
    "${HOOK_TRUST_ARGS[@]}" "${MODEL_ARGS[@]}" "$prompt" \
    > "/out/$tag.jsonl" 2> "/out/$tag.stderr"
  rc=$?
  t1=$(date +%s)
  echo $((t1 - t0)) > "/out/$tag.seconds"
  if [ "$rc" -ne 0 ]; then
    echo "arm=$ARM_TOOL task=$task rc=$rc" >&2
    touch "/out/$tag.failed"
    overall_rc=1
  fi
done
if [ "$REVIEWED_PIXEL_HOOKS" = "1" ] && [ "${ARM_TOOL:-raw}" = "pixel" ]; then
  hook_receipt="/out/pixel-hook-${REP:-1}.jsonl"
  if ! python3 - "$hook_receipt" <<'PY'
import json
import sys
from pathlib import Path

path = Path(sys.argv[1])
try:
    rows = [json.loads(line) for line in path.read_text().splitlines() if line.strip()]
except (OSError, ValueError):
    rows = []
if not rows or any(row.get("response_valid") is not True for row in rows):
    sys.exit(1)
PY
  then
    task="${TASK_LIST[0]:-unknown}"
    echo "Pixel prompt-hook receipt is missing or invalid; excluding this run" >&2
    touch "/out/pixel-${task}-${REP:-1}.failed"
    overall_rc=1
  fi
fi
exit "$overall_rc"
