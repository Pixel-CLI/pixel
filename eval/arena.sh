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
#                      [--results-dir DIR] [--reuse-pixel-image] [--watch]
#                      [--assert-context-parity] [--prepare-pixel-graph]
#                      [--review-pixel-hooks] [--codex-caller-facts]
#   --watch opens one Herdr pane per arm container (when inside Herdr)
#   running `docker exec -it <c> codex` — interactive codex with and
#   without pixel side by side; falls back to a tmux session otherwise.
# Results: <results-dir>/<arm>-<task>-<rep>.jsonl + rank table.
set -euo pipefail
ARENA_DIR="$(cd "$(dirname "$0")" && pwd)"
DOCKER_BIN="$(command -v docker)"
REPO_SNAPSHOT="${REPO_SNAPSHOT:?set REPO_SNAPSHOT to the repo dir to mount at /repo}"
AUTH="${AUTH:-$HOME/.codex/auth.json}"
ARMS="${ARMS:-raw pixel}"
CODEX_MODEL="${CODEX_MODEL:-gpt-5.6-terra}"
CODEX_EFFORT="${CODEX_EFFORT:-medium}"
TASKS="${TASKS:-s1-hook-install s2-vector-recall s3-rename-impact}"
REPS="${REPS:-1}"
WATCH="${WATCH:-0}"
PIXEL_IMAGE_SOURCE="${PIXEL_IMAGE_SOURCE:-build}"
PIXEL_IMAGE_REF="${PIXEL_ARENA_IMAGE:-pixel-arena:pixel}"
RESULTS="${ARENA_RESULTS_DIR:-$ARENA_DIR/arena-results}"
SCENARIOS_DIR="${ARENA_SCENARIOS_DIR:-$ARENA_DIR/scenarios}"
RUN_ID="${RUN_ID:-$(date +%Y%m%d%H%M%S)-$$}"
ASSERT_CONTEXT_PARITY=0
PREPARE_PIXEL_GRAPH="${ARENA_PREPARE_PIXEL_GRAPH:-0}"
REVIEWED_PIXEL_HOOKS="${ARENA_REVIEWED_PIXEL_HOOKS:-0}"
CODEX_CALLER_FACTS="${ARENA_CODEX_CALLER_FACTS:-0}"
START=$(date +%s)

# flags override env: --arms, --tasks, --reps, --results-dir, --watch
while [ $# -gt 0 ]; do
  case "$1" in
    --arms) ARMS="$2"; shift 2 ;;
    --tasks) TASKS="$2"; shift 2 ;;
    --reps) REPS="$2"; shift 2 ;;
    --results-dir) RESULTS="$2"; shift 2 ;;
    --reuse-pixel-image) PIXEL_IMAGE_SOURCE=existing; shift ;;
    --assert-context-parity) ASSERT_CONTEXT_PARITY=1; shift ;;
    --prepare-pixel-graph) PREPARE_PIXEL_GRAPH=1; shift ;;
    --review-pixel-hooks) REVIEWED_PIXEL_HOOKS=1; shift ;;
    --codex-caller-facts) CODEX_CALLER_FACTS=1; shift ;;
    --watch) WATCH=1; shift ;;
    *) echo "unknown arg: $1" >&2; exit 2 ;;
  esac
done

case "$PREPARE_PIXEL_GRAPH" in
  0|1) ;;
  *) echo "ARENA_PREPARE_PIXEL_GRAPH must be 0 or 1" >&2; exit 2 ;;
esac
case "$REVIEWED_PIXEL_HOOKS:$CODEX_CALLER_FACTS" in
  0:0|1:0|1:1) ;;
  *) echo "--codex-caller-facts requires --review-pixel-hooks" >&2; exit 2 ;;
esac
if [ "$PREPARE_PIXEL_GRAPH" = "1" ] && [[ " $ARMS " != *" pixel "* ]]; then
  echo "--prepare-pixel-graph requires the pixel arm" >&2
  exit 2
fi
if [ "$REVIEWED_PIXEL_HOOKS" = "1" ]; then
  read -r -a reviewed_arms <<< "$ARMS"
  if [ "${#reviewed_arms[@]}" -ne 2 ] || \
     ! { [[ " ${reviewed_arms[*]} " == *" raw "* ]] && [[ " ${reviewed_arms[*]} " == *" pixel "* ]]; }; then
    echo "--review-pixel-hooks requires exactly the raw and pixel arms" >&2
    exit 2
  fi
  read -r -a reviewed_tasks <<< "$TASKS"
  if [ "${#reviewed_tasks[@]}" -ne 1 ]; then
    echo "--review-pixel-hooks requires exactly one task so its receipt is unambiguous" >&2
    exit 2
  fi
fi
if [ "$CODEX_CALLER_FACTS" = "1" ] && [ "$PREPARE_PIXEL_GRAPH" != "1" ]; then
  echo "--codex-caller-facts requires --prepare-pixel-graph" >&2
  exit 2
fi

case "$RESULTS" in
  /*) ;;
  *) RESULTS="$PWD/$RESULTS" ;;
esac

if [ -e "$RESULTS" ]; then
  if [ ! -d "$RESULTS" ]; then
    echo "results path is not a directory: $RESULTS" >&2
    exit 2
  fi
  if [ -n "$(find "$RESULTS" -mindepth 1 -maxdepth 1 -print -quit)" ]; then
    echo "refusing to reuse non-empty results directory: $RESULTS" >&2
    echo "choose a new --results-dir (or set ARENA_RESULTS_DIR)" >&2
    exit 2
  fi
else
  mkdir -p "$RESULTS"
fi
if [ ! -d "$SCENARIOS_DIR" ]; then
  echo "scenario directory does not exist: $SCENARIOS_DIR" >&2
  exit 2
fi
for task in $TASKS; do
  if [ ! -f "$SCENARIOS_DIR/$task.json" ]; then
    echo "selected scenario does not exist: $SCENARIOS_DIR/$task.json" >&2
    exit 2
  fi
done
python3 - "$ARENA_DIR" "$RESULTS" <<'PY'
import hashlib
import json
import sys
from pathlib import Path

arena_dir, results_dir = map(Path, sys.argv[1:])
files = {
    "arena_runner": (arena_dir / "arena.sh", None),
    "entrypoint": (arena_dir / "arena/entrypoint.sh", "/usr/local/bin/arena-entrypoint"),
    "context_manifest": (arena_dir / "arena/context_manifest.py", "/usr/local/lib/arena-context-manifest.py"),
    "hook_audit": (arena_dir / "arena/hook_audit.py", "/usr/local/lib/arena-hook-audit.py"),
}
receipt = {
    name: {
        "source_path": str(path.resolve()),
        "container_path": container_path,
        "sha256": hashlib.sha256(path.read_bytes()).hexdigest(),
    }
    for name, (path, container_path) in files.items()
}
(results_dir / "harness-source.json").write_text(json.dumps(receipt, indent=2) + "\n")
PY
: > "$RESULTS/.watch-panes"

prompt_for() { python3 -c "import json,sys;print(json.load(open(sys.argv[1]))['prompt'])" "$SCENARIOS_DIR/$1.json"; }

launch_arm() {  # arm rep immutable-image-id — one container runs all tasks
  local arm="$1" rep="$2" image_id="$3" actual_image container arm_caller_facts=0
  if [ "$arm" = "pixel" ]; then arm_caller_facts="$CODEX_CALLER_FACTS"; fi
  container="arena-$arm-$RUN_ID-$rep"
  local snap="$RESULTS/snapshot-$arm-$rep"
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
      if [ -f "$snap/AGENTS.md" ]; then
        if sed --version >/dev/null 2>&1; then
          sed -i '/<!-- pixel:warp-retrieval:begin -->/,/<!-- pixel:warp-retrieval:end -->/d' "$snap/AGENTS.md"
        else
          sed -i '' '/<!-- pixel:warp-retrieval:begin -->/,/<!-- pixel:warp-retrieval:end -->/d' "$snap/AGENTS.md"
        fi
        grep -vEi 'pixel' "$snap/AGENTS.md" > "$snap/AGENTS.md.scrubbed" \
          && mv "$snap/AGENTS.md.scrubbed" "$snap/AGENTS.md"
      fi
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
  "$DOCKER_BIN" create --name "$container" \
    -v "$snap":/repo \
  -v "$AUTH":/root/.codex/auth.json:ro \
  -v "$ARENA_DIR/arena/entrypoint.sh":/usr/local/bin/arena-entrypoint:ro \
  -v "$ARENA_DIR/arena/context_manifest.py":/usr/local/lib/arena-context-manifest.py:ro \
  -v "$ARENA_DIR/arena/hook_audit.py":/usr/local/lib/arena-hook-audit.py:ro \
    -v "$SCENARIOS_DIR":/prompts:ro \
    -v "$RESULTS":/out \
    -e ARM_TOOL="$arm" -e REP="$rep" -e TASKS="$TASKS" -e ARENA_RUN_ID="$RUN_ID" \
    -e PIXEL_ARENA_PREP_GRAPH="$PREPARE_PIXEL_GRAPH" \
    -e ARENA_REVIEWED_PIXEL_HOOKS="$REVIEWED_PIXEL_HOOKS" \
    -e ARENA_CODEX_CALLER_FACTS="$arm_caller_facts" \
    -e CODEX_MODEL="${CODEX_MODEL:-}" -e CODEX_EFFORT="${CODEX_EFFORT:-}" \
    "$image_id" >/dev/null
  actual_image=$("$DOCKER_BIN" inspect --format '{{.Image}}' "$container")
  if ! python3 - "$arm" "$image_id" "$actual_image" "$RESULTS/container-image-$arm-$rep.json" <<'PY'
import json
import sys
from pathlib import Path

arm, expected, actual, output = sys.argv[1:]
Path(output).write_text(json.dumps({
    "arm": arm,
    "expected_image_id": expected,
    "actual_container_image_id": actual,
    "matches": expected == actual,
}, indent=2) + "\n")
if expected != actual:
    sys.exit(f"container image mismatch: arm={arm} expected={expected} actual={actual}")
PY
  then
    "$DOCKER_BIN" rm "$container" >/dev/null
    return 1
  fi
  "$DOCKER_BIN" start "$container" >/dev/null
  echo "launched arm=$arm container=$container image=$actual_image"
}

ARM_IMAGE_IDS=()
PIXEL_IMAGE_ID=""
CODEX_VERSION=""
for arm in $ARMS; do
  docker_build="pixel-arena:$arm"
  if [ "$arm" = "pixel" ]; then
    docker_build="$PIXEL_IMAGE_REF"
    if [ "$PIXEL_IMAGE_SOURCE" = "existing" ]; then
      if ! "$DOCKER_BIN" image inspect "$docker_build" >/dev/null 2>&1; then
        echo "requested existing Pixel image is missing: $docker_build" >&2
        exit 2
      fi
      echo "=== using existing image $docker_build (source pinned by caller)"
    elif [ "${PIXEL_SRC:-git}" = "local" ]; then
      # PIXEL_SRC=local builds this repo's working tree (PIXEL_SRC_DIR
      # override), including unpushed candidate behavior.
      # pixel source context is the repo hosting this script — not
      # REPO_SNAPSHOT, which is the repo under test (may be any project)
      pixel_src_dir="${PIXEL_SRC_DIR:-$(cd "$ARENA_DIR/.." && pwd)}"
      echo "=== building image $docker_build (local: $pixel_src_dir)"
      "$DOCKER_BIN" build -f "$pixel_src_dir/eval/arena/Dockerfile.pixel" -t "$docker_build" \
        --build-arg PIXEL_SRC=local \
        --build-arg ENTRYPOINT_SRC=eval/arena/entrypoint.sh "$pixel_src_dir" \
        || { echo "IMAGE BUILD FAILED: $arm"; exit 1; }
    else
      # PIXEL_SRC=git: pin remote refs/heads/main so the layer cache busts
      # exactly when main moves.
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
  image_id=$("$DOCKER_BIN" image inspect --format '{{.Id}}' "$docker_build")
  ARM_IMAGE_IDS+=("$image_id")
  if [ "$arm" = "pixel" ]; then PIXEL_IMAGE_ID="$image_id"; fi
  image_codex_version=$("$DOCKER_BIN" run --rm --entrypoint codex "$image_id" --version)
  if [ -n "$CODEX_VERSION" ] && [ "$image_codex_version" != "$CODEX_VERSION" ]; then
    echo "Codex versions differ across selected images: $CODEX_VERSION versus $image_codex_version ($arm)" >&2
    exit 2
  fi
  CODEX_VERSION="$image_codex_version"
done

# all arms in parallel: one container each, all tasks inside
CONTAINERS=()
CONTAINER_ARMS=()
CONTAINER_REPS=()
for rep in $(seq 1 "$REPS"); do
  # fresh snapshot per rep: prior reps' index artifacts and tool edits must
  # not leak into the next rep's starting state. Rep-scoped names —
  # deleting a shared snapshot while the previous rep's containers still
  # mount it races and kills their later tasks.
  for arm in $ARMS; do rm -rf "$RESULTS/snapshot-$arm-$rep" || true; done
  rep_containers=()
  arm_index=0
  for arm in $ARMS; do
    launch_arm "$arm" "$rep" "${ARM_IMAGE_IDS[$arm_index]}"
    arm_index=$((arm_index + 1))
    CONTAINERS+=("arena-$arm-$RUN_ID-$rep")
    CONTAINER_ARMS+=("$arm")
    CONTAINER_REPS+=("$rep")
    rep_containers+=("arena-$arm-$RUN_ID-$rep")
  done
  # serialize reps: waiting here keeps rep N+1 from racing rep N's still
  # running containers for CPU — wall times stay comparable across reps
  "$DOCKER_BIN" wait "${rep_containers[@]}" >/dev/null 2>&1 || true
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
for i in "${!CONTAINERS[@]}"; do
  c="${CONTAINERS[$i]}"
  docker wait "$c" >/dev/null 2>&1 || FAIL=1
  rc=$(docker inspect -f '{{.State.ExitCode}}' "$c" 2>/dev/null || echo "?")
  logs=$(docker logs "$c" 2>&1 | grep -E "rc=[1-9]|not found|Error" | tail -2 || true)
  [ -n "$logs" ] && echo "[$c] $logs"
  if [ "$rc" != "0" ]; then
    FAIL=1
    for task in $TASKS; do
      touch "$RESULTS/${CONTAINER_ARMS[$i]}-$task-${CONTAINER_REPS[$i]}.failed"
    done
  fi
  docker rm "$c" >/dev/null 2>&1
done
[ "$FAIL" -ne 0 ] && echo "WARNING: some arm containers failed (see above)"

echo "=== ranking"
rank_results() {
  # ARMS and TASKS are intentionally space-delimited CLI selections.
  # shellcheck disable=SC2086
  python3 "$ARENA_DIR/arena/rank.py" --results "$RESULTS" \
    --scenarios-dir "$SCENARIOS_DIR" --arms $ARMS --tasks $TASKS --reps "$REPS" \
    "$@" \
    --run-id "$RUN_ID" --model "$CODEX_MODEL" --effort "$CODEX_EFFORT" \
    --repo-snapshot "$REPO_SNAPSHOT" --pixel-image-id "$PIXEL_IMAGE_ID" \
    --pixel-source-id "${PIXEL_SOURCE_ID:-${PIXEL_SRC:-git}}" \
    --codex-version "$CODEX_VERSION"
}
if [ "$ASSERT_CONTEXT_PARITY" = "1" ]; then
  rank_results --assert-context-parity
else
  rank_results
fi
echo "total wall: $(( $(date +%s) - START ))s"
[ "$FAIL" -eq 0 ] || exit "$FAIL"
