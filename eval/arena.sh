#!/usr/bin/env bash
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

# eval/arena.sh — head-to-head: raw codex vs retrieval-tool arms, ranked.
#
# Arms: raw | semble | graft | stacklit | gitnexus | gortex | pixel
# Each arm = one docker image (eval/arena/Dockerfile.<arm>, FROM the shared
# pixel-arena-base) with its tool installed and wired to codex per that tool's
# own codex docs. The repo snapshot, auth, model, sandbox, and prompts are
# identical across arms.
#
# Usage: eval/arena.sh [--arms "raw pixel"] [--tasks "s1 s2 s3"] [--reps N]
# Results: eval/arena-results/<arm>-<task>-<rep>.jsonl + rank table.
set -euo pipefail
ARENA_DIR="$(cd "$(dirname "$0")" && pwd)"
DOCKER="$_"   # placeholder; resolved below to bypass shell wrappers
DOCKER_BIN="$(command -v docker)"
REPO_SNAPSHOT="${REPO_SNAPSHOT:?set REPO_SNAPSHOT to the repo dir to mount at /repo}"
AUTH="${AUTH:-$HOME/.codex/auth.json}"
ARMS="${ARMS:-raw semble graft stacklit gitnexus gortex pixel}"
CODEX_MODEL="${CODEX_MODEL:-gpt-6-luna}"
CODEX_EFFORT="${CODEX_EFFORT:-high}"
TASKS="${TASKS:-s1-hook-install s2-vector-recall s3-rename-impact}"
REPS="${REPS:-1}"
RESULTS="$ARENA_DIR/arena-results"
mkdir -p "$RESULTS"
START=$(date +%s)

# flags override env: --arms, --tasks, --reps
while [ $# -gt 0 ]; do
  case "$1" in
    --arms) ARMS="$2"; shift 2 ;;
    --tasks) TASKS="$2"; shift 2 ;;
    --reps) REPS="$2"; shift 2 ;;
    *) echo "unknown arg: $1" >&2; exit 2 ;;
  esac
done

prompt_for() { python3 -c "import json,sys;print(json.load(open(sys.argv[1]))['prompt'])" "$ARENA_DIR/scenarios/$1.json"; }

launch_arm() {  # arm rep — one container runs all tasks
  local arm="$1" rep="$2"
  local snap="$RESULTS/snapshot-$arm"
  [ -d "$snap" ] || git clone -q "$REPO_SNAPSHOT" "$snap"
  local missing=0
  for task in $TASKS; do
    [ -s "$RESULTS/$arm-$task-$rep.jsonl" ] || missing=1
  done
  [ "$missing" -eq 0 ] && { echo "skip arm=$arm rep=$rep (all tasks exist)"; return 0; }
  "$DOCKER_BIN" run -d --name "arena-$arm-$rep-$$" \
    -v "$snap":/repo \
    -v "$AUTH":/root/.codex/auth.json:ro \
    -v "$ARENA_DIR/scenarios":/prompts:ro \
    -v "$RESULTS":/out \
    -e ARM_TOOL="$arm" -e REP="$rep" -e TASKS="$TASKS" \
    -e CODEX_MODEL="${CODEX_MODEL:-}" -e CODEX_EFFORT="${CODEX_EFFORT:-}" \
    "pixel-arena:$arm" >/dev/null
  echo "launched arm=$arm container=arena-$arm-$rep-$$"
}

for arm in $ARMS; do
  docker_build="pixel-arena:$arm"
  if ! "$DOCKER_BIN" image inspect "$docker_build" >/dev/null 2>&1; then
    echo "=== building image $docker_build"
    "$DOCKER_BIN" build -f "$ARENA_DIR/arena/Dockerfile.$arm" -t "$docker_build" "$ARENA_DIR/arena" || { echo "IMAGE BUILD FAILED: $arm"; exit 1; }
  fi
done

# all arms in parallel: one container each, all tasks inside
CONTAINERS=()
for rep in $(seq 1 "$REPS"); do
  # fresh snapshot per rep: prior reps' index artifacts and tool edits must
  # not leak into the next rep's starting state
  for arm in $ARMS; do rm -rf "$RESULTS/snapshot-$arm"; done
  for arm in $ARMS; do
    launch_arm "$arm" "$rep"
    CONTAINERS+=("arena-$arm-$rep-$$")
  done
done
FAIL=0
for c in "${CONTAINERS[@]}"; do
  docker wait "$c" >/dev/null 2>&1 || FAIL=1
  rc=$(docker inspect -f '{{.State.ExitCode}}' "$c" 2>/dev/null || echo "?")
  logs=$(docker logs "$c" 2>&1 | grep -E "rc=[1-9]|not found|Error" | tail -2)
  [ -n "$logs" ] && echo "[$c] $logs"
  [ "$rc" != "0" ] && FAIL=1
  docker rm "$c" >/dev/null 2>&1
done
[ "$FAIL" -ne 0 ] && echo "WARNING: some arm containers failed (see above)"

echo "=== ranking"
python3 "$ARENA_DIR/arena/rank.py" --results "$RESULTS" --scenarios-dir "$ARENA_DIR/scenarios" --arms $ARMS
echo "total wall: $(( $(date +%s) - START ))s"
