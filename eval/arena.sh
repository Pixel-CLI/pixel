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
# Usage: eval/arena.sh [--arms "raw pixel"] [--tasks "s1 s2 s3"] [--reps N] [--watch]
#   --watch opens one Herdr pane per arm container (when inside Herdr)
#   running `docker exec -it <c> codex` — interactive codex with and
#   without pixel side by side; falls back to a tmux session otherwise.
# Results: eval/arena-results/<arm>-<task>-<rep>.jsonl + rank table.
set -euo pipefail
ARENA_DIR="$(cd "$(dirname "$0")" && pwd)"
DOCKER="$_"   # placeholder; resolved below to bypass shell wrappers
DOCKER_BIN="$(command -v docker)"
REPO_SNAPSHOT="${REPO_SNAPSHOT:?set REPO_SNAPSHOT to the repo dir to mount at /repo}"
AUTH="${AUTH:-$HOME/.codex/auth.json}"
ARMS="${ARMS:-raw pixel}"
CODEX_MODEL="${CODEX_MODEL:-gpt-5.6-terra}"
CODEX_EFFORT="${CODEX_EFFORT:-medium}"
TASKS="${TASKS:-s1-hook-install s2-vector-recall s3-rename-impact}"
REPS="${REPS:-1}"
WATCH="${WATCH:-0}"
RESULTS="$ARENA_DIR/arena-results"
mkdir -p "$RESULTS"
START=$(date +%s)

# flags override env: --arms, --tasks, --reps, --watch
while [ $# -gt 0 ]; do
  case "$1" in
    --arms) ARMS="$2"; shift 2 ;;
    --tasks) TASKS="$2"; shift 2 ;;
    --reps) REPS="$2"; shift 2 ;;
    --watch) WATCH=1; shift ;;
    *) echo "unknown arg: $1" >&2; exit 2 ;;
  esac
done

# Restart hygiene: a previous run's containers and --watch panes are stale —
# close/remove them before launching new ones.
for stale in $(docker ps -aq --filter "name=^arena-" 2>/dev/null); do
  docker rm -f "$stale" >/dev/null 2>&1 && echo "removed stale container $stale"
done
if [ -f "$RESULTS/.watch-panes" ] && command -v herdr >/dev/null 2>&1; then
  while IFS= read -r old_pane; do
    herdr pane close "$old_pane" >/dev/null 2>&1 && echo "closed stale pane $old_pane"
  done < "$RESULTS/.watch-panes"
fi
: > "$RESULTS/.watch-panes"

prompt_for() { python3 -c "import json,sys;print(json.load(open(sys.argv[1]))['prompt'])" "$ARENA_DIR/scenarios/$1.json"; }

launch_arm() {  # arm rep — one container runs all tasks
  local arm="$1" rep="$2"
  local snap="$RESULTS/snapshot-$arm"
  if [ ! -d "$snap" ]; then
    git clone -q "$REPO_SNAPSHOT" "$snap"
    if [ "$arm" = "raw" ]; then
      # The snapshot carries pixel's own agent-facing docs; raw must not see
      # them or codex tries `pixel …` and burns a turn on the failure.
      rm -rf "$snap/.agents/skills/pixel" "$snap/skills/pixel" \
        "$snap/.openclaw/skills/pixel" "$snap/rules/pixel.md" \
        "$snap/PIXEL.md" "$snap/PIXEL-SUBAGENT.md" "$snap/.cursor/rules/pixel.mdc" \
        "$snap/.windsurf/rules/pixel.md" "$snap/.kiro/steering/pixel.md" \
        "$snap/.qoder/rules/pixel.md" "$snap/.clinerules/pixel.md"
      # AGENTS.md is this repo's own dev doc and teaches pixel retrieval
      # (managed warp-retrieval block + reinstall/doctor commands). Remove
      # the managed block, then every remaining line that names pixel —
      # snapshot-only edit; the eval loses project context it doesn't need.
      sed -i '' '/<!-- pixel:warp-retrieval:begin -->/,/<!-- pixel:warp-retrieval:end -->/d' "$snap/AGENTS.md" 2>/dev/null \
        || sed -i '/<!-- pixel:warp-retrieval:begin -->/,/<!-- pixel:warp-retrieval:end -->/d' "$snap/AGENTS.md"
      grep -vEi 'pixel' "$snap/AGENTS.md" > "$snap/AGENTS.md.scrubbed" \
        && mv "$snap/AGENTS.md.scrubbed" "$snap/AGENTS.md"
      rm -rf "$snap/.agents/skills/pixel-retro"
      for f in "$snap"/.agents/rules/*.md "$snap"/.agents/skills/*/SKILL.md; do
        [ -f "$f" ] || continue
        grep -vEi 'pixel' "$f" > "$f.scrubbed" && mv "$f.scrubbed" "$f"
      done
    fi
  fi
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
  if [ "$arm" = "pixel" ]; then
    # PIXEL_SRC=git (default): pin remote refs/heads/main so the layer cache
    # busts exactly when main moves. PIXEL_SRC=local: build the source in the
    # build context instead — arena.sh passes REPO_SNAPSHOT as the context,
    # so the arm measures the local tree (including unpushed work).
    if [ "${PIXEL_SRC:-git}" = "local" ]; then
      echo "=== building image $docker_build (local: $REPO_SNAPSHOT)"
      "$DOCKER_BIN" build -f "$ARENA_DIR/arena/Dockerfile.pixel" -t "$docker_build" \
        --build-arg PIXEL_SRC=local \
        --build-arg ENTRYPOINT_SRC=eval/arena/entrypoint.sh "$REPO_SNAPSHOT" \
        || { echo "IMAGE BUILD FAILED: $arm"; exit 1; }
    else
      pixel_main_sha=$(git ls-remote https://github.com/Pixel-CLI/pixel refs/heads/main | cut -f1)
      echo "=== building image $docker_build (main: ${pixel_main_sha:-unresolved})"
      "$DOCKER_BIN" build -f "$ARENA_DIR/arena/Dockerfile.pixel" -t "$docker_build" \
        --build-arg "PIXEL_MAIN_SHA=${pixel_main_sha:-main}" "$ARENA_DIR/arena" \
        || { echo "IMAGE BUILD FAILED: $arm"; exit 1; }
    fi
  elif ! "$DOCKER_BIN" image inspect "$docker_build" >/dev/null 2>&1; then
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

# --watch: one pane per container streaming `docker logs -f`. Inside Herdr
# (HERDR_ENV=1) splits the current pane right, side by side with this one;
# otherwise falls back to a tiled tmux session you attach separately.
if [ "$WATCH" = "1" ]; then
  if [ -n "${HERDR_ENV:-}" ] && command -v herdr >/dev/null 2>&1; then
    for c in "${CONTAINERS[@]}"; do
      watch_pane=$(herdr pane split --current --direction right \
        | python3 -c "import json,sys;print(json.load(sys.stdin)['result']['pane']['pane_id'])" 2>/dev/null || true)
      if [ -n "$watch_pane" ]; then
        echo "$watch_pane" >> "$RESULTS/.watch-panes"
        watch_arm=${c#arena-}; watch_arm=${watch_arm%%-*}
        herdr pane rename "$watch_pane" "arena-$watch_arm" >/dev/null 2>&1
        # interactive codex inside the arm's container: raw pane is bare
        # codex, pixel pane has pixel installed+indexed by the entrypoint.
        herdr pane run "$watch_pane" \
          "docker exec -it $c codex -m $CODEX_MODEL -c model_reasoning_effort=$CODEX_EFFORT"
        # codex asks to trust /repo, then to trust installed hooks (pixel
        # arm): answer both so the pane lands on the prompt, unattended.
        (
          pane_id="$watch_pane"
          for _ in $(seq 1 45); do
            screen=$(herdr pane read "$pane_id" 2>/dev/null | tail -20)
            case "$screen" in
              *"Trust and continue"*)
                herdr pane send-keys "$pane_id" Enter >/dev/null 2>&1 ;;
              *"Trust all and continue"*)
                herdr pane send-keys "$pane_id" Down >/dev/null 2>&1
                sleep 1
                herdr pane send-keys "$pane_id" Enter >/dev/null 2>&1
                exit 0 ;;
              *"Ask Codex"*) exit 0 ;;
            esac
            sleep 2
          done
        ) &
      else
        echo "WARNING: herdr pane split failed; $c logs via 'docker logs -f $c'" >&2
      fi
    done
    echo "watching: herdr panes running interactive codex (${#CONTAINERS[@]} containers)"
  elif ! command -v tmux >/dev/null 2>&1; then
    echo "WARNING: --watch needs herdr (HERDR_ENV) or tmux; neither found" >&2
  elif [ "${#CONTAINERS[@]}" -gt 0 ]; then
    WATCH_SESSION="arena-$$"
    tmux new-session -d -s "$WATCH_SESSION" -x 220 -y 50 \
      "docker logs -f ${CONTAINERS[0]}; echo; echo 'container exited'; exec \${SHELL:-sh}"
    for c in "${CONTAINERS[@]:1}"; do
      tmux split-window -t "$WATCH_SESSION" \
        "docker logs -f $c; echo; echo 'container exited'; exec \${SHELL:-sh}"
      tmux select-layout -t "$WATCH_SESSION" tiled >/dev/null
    done
    tmux select-layout -t "$WATCH_SESSION" tiled >/dev/null
    echo "watching: tmux attach -t $WATCH_SESSION"
  fi
fi

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
