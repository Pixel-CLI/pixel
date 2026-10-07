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
#                      [--review-pixel-hooks] [--skill-candidate-dir DIR]
#   --watch opens one Herdr pane per isolated watch container (when inside Herdr)
#   running `docker exec -it <watch-c> codex` — interactive codex with and
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
SKILL_CANDIDATE_DIR="${ARENA_SKILL_CANDIDATE_DIR:-}"
SKILL_NAME=""
SKILL_SOURCE_SHA256=""
SKILL_IMAGE_READY=0
START=$(date +%s)
if [ -n "${ARENA_CODEX_CALLER_FACTS:-}" ] && [ "${ARENA_CODEX_CALLER_FACTS}" != "0" ]; then
  echo "ARENA_CODEX_CALLER_FACTS is retired; use a skill-only candidate evaluation" >&2
  exit 2
fi

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
    --skill-candidate-dir) SKILL_CANDIDATE_DIR="$2"; shift 2 ;;
    --codex-caller-facts)
      echo "--codex-caller-facts is retired; use a skill-only candidate evaluation" >&2
      exit 2
      ;;
    --watch) WATCH=1; shift ;;
    *) echo "unknown arg: $1" >&2; exit 2 ;;
  esac
done

case "$PREPARE_PIXEL_GRAPH" in
  0|1) ;;
  *) echo "ARENA_PREPARE_PIXEL_GRAPH must be 0 or 1" >&2; exit 2 ;;
esac
case "$REVIEWED_PIXEL_HOOKS" in 0|1) ;; *) echo "ARENA_REVIEWED_PIXEL_HOOKS must be 0 or 1" >&2; exit 2 ;; esac
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

if [ "$REVIEWED_PIXEL_HOOKS" != "1" ] && [ " $ARMS " = " raw pixel " ]; then
  echo "WARNING: live Pixel brief evidence is disabled; rerun with --review-pixel-hooks and a reviewed hook image" >&2
fi
SKILL_PILOT=0
if [ -n "$SKILL_CANDIDATE_DIR" ]; then
  SKILL_PILOT=1
  if [ ! -d "$SKILL_CANDIDATE_DIR" ]; then
    echo "skill candidate directory does not exist: $SKILL_CANDIDATE_DIR" >&2
    exit 2
  fi
  SKILL_CANDIDATE_DIR="$(cd "$SKILL_CANDIDATE_DIR" && pwd)"
  read -r -a skill_arms <<< "$ARMS"
  if [ "${#skill_arms[@]}" -ne 2 ] || \
     ! { [[ " ${skill_arms[*]} " == *" raw "* ]] && [[ " ${skill_arms[*]} " == *" pixel "* ]]; }; then
    echo "--skill-candidate-dir requires exactly the raw and pixel arms" >&2
    exit 2
  fi
  if [ "$REVIEWED_PIXEL_HOOKS" = "1" ]; then
    echo "skill-only runs do not install or audit Pixel hooks; omit --review-pixel-hooks" >&2
    exit 2
  fi
  if [ "$ASSERT_CONTEXT_PARITY" = "1" ]; then
    echo "skill-only runs use --assert-skill-only; omit --assert-context-parity" >&2
    exit 2
  fi
  skill_source_receipt=$(python3 "$ARENA_DIR/arena/skill_candidate.py" inspect \
    --source "$SKILL_CANDIDATE_DIR")
  SKILL_NAME=$(python3 -c 'import json,sys;print(json.loads(sys.argv[1])["skill_name"])' "$skill_source_receipt")
  SKILL_SOURCE_SHA256=$(python3 -c 'import json,sys;print(json.loads(sys.argv[1])["source_sha256"])' "$skill_source_receipt")
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
    "skill_candidate": (arena_dir / "arena/skill_candidate.py", "/usr/local/lib/arena-skill-candidate.py"),
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

scrub_pixel_lines() {
  local path="$1" temp="${1}.scrubbed" status
  if grep -vEi 'pixel' "$path" > "$temp"; then
    :
  else
    status=$?
    if [ "$status" -ne 1 ]; then
      rm -f "$temp" || true
      return "$status"
    fi
  fi
  if ! mv "$temp" "$path"; then
    rm -f "$temp" || true
    return 1
  fi
}

launch_arm() {  # arm rep immutable-image-id — one container runs all tasks
  local arm="$1" rep="$2" image_id="$3" actual_image container
  container="arena-$arm-$RUN_ID-$rep"
  local snap="$RESULTS/snapshot-$arm-$rep"
  if [ ! -d "$snap" ]; then
    if ! git clone -q "$REPO_SNAPSHOT" "$snap"; then
      echo "snapshot clone failed: arm=$arm rep=$rep" >&2
      rm -rf "$snap"
      return 1
    fi
    if [ "$arm" = "raw" ] || [ "$SKILL_PILOT" = "1" ]; then
      # The snapshot carries pixel's own agent-facing docs; raw must not see
      # them or codex tries `pixel …` and burns a turn on the failure.
      if ! rm -rf "$snap/.agents/skills/pixel" "$snap/skills/pixel" \
        "$snap/.openclaw/skills/pixel" "$snap/rules/pixel.md" \
        "$snap/PIXEL.md" "$snap/PIXEL-SUBAGENT.md" "$snap/.cursor/rules/pixel.mdc" \
        "$snap/.windsurf/rules/pixel.md" "$snap/.kiro/steering/pixel.md" \
        "$snap/.qoder/rules/pixel.md" "$snap/.clinerules/pixel.md"; then
        echo "could not remove Pixel snapshot context: arm=$arm rep=$rep" >&2
        return 1
      fi
      # AGENTS.md is this repo's own dev doc and teaches pixel retrieval
      # (managed warp-retrieval block + reinstall/doctor commands). Remove
      # the managed block, then every remaining line that names pixel —
      # snapshot-only edit; the eval loses project context it doesn't need.
      if [ -f "$snap/AGENTS.md" ]; then
        if sed --version >/dev/null 2>&1; then
          if ! sed -i '/<!-- pixel:warp-retrieval:begin -->/,/<!-- pixel:warp-retrieval:end -->/d' "$snap/AGENTS.md"; then
            echo "could not remove managed Pixel block from AGENTS.md: arm=$arm rep=$rep" >&2
            return 1
          fi
        else
          if ! sed -i '' '/<!-- pixel:warp-retrieval:begin -->/,/<!-- pixel:warp-retrieval:end -->/d' "$snap/AGENTS.md"; then
            echo "could not remove managed Pixel block from AGENTS.md: arm=$arm rep=$rep" >&2
            return 1
          fi
        fi
        if ! scrub_pixel_lines "$snap/AGENTS.md"; then
          echo "could not scrub Pixel references from AGENTS.md: arm=$arm rep=$rep" >&2
          return 1
        fi
      fi
      if ! rm -rf "$snap/.agents/skills/pixel-retro" "$snap/.agents/skills/pixel-impact"; then
        echo "could not remove Pixel skill snapshots: arm=$arm rep=$rep" >&2
        return 1
      fi
      if [ -n "$SKILL_NAME" ] && ! rm -rf "$snap/.agents/skills/$SKILL_NAME"; then
        echo "could not remove candidate skill from baseline snapshot: arm=$arm rep=$rep" >&2
        return 1
      fi
      for f in "$snap"/.agents/rules/*.md "$snap"/.agents/skills/*/SKILL.md; do
        [ -f "$f" ] || continue
        if ! scrub_pixel_lines "$f"; then
          echo "could not scrub Pixel references from $f: arm=$arm rep=$rep" >&2
          return 1
        fi
      done
    fi
    if [ "$SKILL_PILOT" = "1" ]; then
      if ! python3 "$ARENA_DIR/arena/skill_candidate.py" stage \
        --source "$SKILL_CANDIDATE_DIR" --repo "$snap" --arm "$arm" \
        --receipt "$RESULTS/skill-stage-$arm-$rep.json"; then
        echo "skill staging failed: arm=$arm rep=$rep" >&2
        return 1
      fi
      if ! SKILL_NAME=$(python3 -c 'import json,sys;print(json.load(open(sys.argv[1]))["skill_name"])' \
        "$RESULTS/skill-stage-$arm-$rep.json"); then
        echo "could not read staged skill identity: arm=$arm rep=$rep" >&2
        return 1
      fi
    fi
    if [ "$arm" = "pixel" ]; then
      # Reviewed hook experiments must not inherit a repo-local Codex hook
      # file from the source checkout; the disposable user-level registry is
      # the only hook surface under audit.
      rm -f "$snap/.codex/hooks.json" "$snap/.codex/pixel-composed-guard-backup.json"
    fi
  fi
  if [ "$WATCH" = "1" ]; then
    rm -rf "$RESULTS/watch-snapshot-$arm-$rep" "$RESULTS/watch-out-$arm-$rep"
    cp -a "$snap" "$RESULTS/watch-snapshot-$arm-$rep"
    mkdir -p "$RESULTS/watch-out-$arm-$rep"
  fi
  local missing=0
  for task in $TASKS; do
    [ -s "$RESULTS/$arm-$task-$rep.jsonl" ] || missing=1
  done
  [ "$missing" -eq 0 ] && { echo "skip arm=$arm rep=$rep (all tasks exist)"; return 0; }
  if ! "$DOCKER_BIN" create --name "$container" \
    -v "$snap":/repo \
  -v "$AUTH":/root/.codex/auth.json:ro \
  -v "$ARENA_DIR/arena/entrypoint.sh":/usr/local/bin/arena-entrypoint:ro \
    -v "$ARENA_DIR/arena/context_manifest.py":/usr/local/lib/arena-context-manifest.py:ro \
    -v "$ARENA_DIR/arena/hook_audit.py":/usr/local/lib/arena-hook-audit.py:ro \
    -v "$ARENA_DIR/arena/skill_candidate.py":/usr/local/lib/arena-skill-candidate.py:ro \
    -v "$SCENARIOS_DIR":/prompts:ro \
    -v "$RESULTS":/out \
    -e ARM_TOOL="$arm" -e REP="$rep" -e TASKS="$TASKS" -e ARENA_RUN_ID="$RUN_ID" \
    -e PIXEL_ARENA_PREP_GRAPH="$PREPARE_PIXEL_GRAPH" \
    -e PIXEL_ARENA_REUSE_INDEX="${PIXEL_ARENA_REUSE_INDEX:-1}" \
    -e ARENA_SKILL_PILOT="$SKILL_PILOT" \
    -e ARENA_REVIEWED_PIXEL_HOOKS="$REVIEWED_PIXEL_HOOKS" \
    -e CODEX_MODEL="${CODEX_MODEL:-}" -e CODEX_EFFORT="${CODEX_EFFORT:-}" \
    "$image_id" >/dev/null; then
    echo "container create failed: arm=$arm rep=$rep" >&2
    "$DOCKER_BIN" rm -f "$container" >/dev/null 2>&1 || true
    return 1
  fi
  if ! actual_image=$("$DOCKER_BIN" inspect --format '{{.Image}}' "$container"); then
    echo "container image inspection failed: arm=$arm rep=$rep" >&2
    "$DOCKER_BIN" rm -f "$container" >/dev/null 2>&1 || true
    return 1
  fi
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
    "$DOCKER_BIN" rm -f "$container" >/dev/null 2>&1 || true
    return 1
  fi
  if ! "$DOCKER_BIN" start "$container" >/dev/null; then
    echo "container start failed: arm=$arm rep=$rep" >&2
    "$DOCKER_BIN" rm -f "$container" >/dev/null 2>&1 || true
    return 1
  fi
  echo "launched arm=$arm container=$container image=$actual_image"
}

ARM_IMAGE_IDS=()
PIXEL_IMAGE_ID=""
CODEX_VERSION=""
for arm in $ARMS; do
  docker_build="pixel-arena:$arm"
  if [ "$SKILL_PILOT" = "1" ]; then
    docker_build="$PIXEL_IMAGE_REF"
    if [ "$SKILL_IMAGE_READY" = "1" ]; then
      : # Both skill-only arms intentionally share the already-resolved image.
    elif [ "$PIXEL_IMAGE_SOURCE" = "existing" ]; then
      if ! "$DOCKER_BIN" image inspect "$docker_build" >/dev/null 2>&1; then
        echo "requested existing Pixel image is missing: $docker_build" >&2
        exit 2
      fi
      echo "=== using existing image $docker_build for both skill-pilot arms"
    elif [ "${PIXEL_SRC:-git}" = "local" ]; then
      pixel_src_dir="${PIXEL_SRC_DIR:-$(cd "$ARENA_DIR/.." && pwd)}"
      echo "=== building shared skill-pilot image $docker_build (local: $pixel_src_dir)"
      "$DOCKER_BIN" build -f "$pixel_src_dir/eval/arena/Dockerfile.pixel" -t "$docker_build" \
        --build-arg PIXEL_SRC=local --build-arg ENTRYPOINT_SRC=eval/arena/entrypoint.sh "$pixel_src_dir" \
        || { echo "IMAGE BUILD FAILED: skill-pilot"; exit 1; }
    else
      pixel_main_sha=$(git ls-remote https://github.com/Pixel-CLI/pixel refs/heads/main | cut -f1)
      echo "=== building shared skill-pilot image $docker_build (main: ${pixel_main_sha:-unresolved})"
      "$DOCKER_BIN" build -f "$ARENA_DIR/arena/Dockerfile.pixel" -t "$docker_build" \
        --build-arg "PIXEL_MAIN_SHA=${pixel_main_sha:-main}" "$ARENA_DIR/arena" \
        || { echo "IMAGE BUILD FAILED: skill-pilot"; exit 1; }
    fi
    SKILL_IMAGE_READY=1
  elif [ "$arm" = "pixel" ]; then
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

# --review-pixel-hooks requires the pixel image's install to write the full
# audited 11-command Codex hook set. Pixel main's native-default install
# writes task-event hooks only (no `run-hook prompt-submit`, no metrics), which
# the paired audit rejects after containers have already spent minutes on
# preparation. Probe the image once here, so the failure is immediate.
if [ "$REVIEWED_PIXEL_HOOKS" = "1" ]; then
  case " $ARMS " in *" pixel "*) ;;
    *) echo "--review-pixel-hooks needs a pixel arm" >&2; exit 2 ;;
  esac
  pixel_image="${PIXEL_IMAGE_ID:-${PIXEL_IMAGE_REF:-pixel-arena:pixel}}"
  if ! "$DOCKER_BIN" run --rm \
      -v "$ARENA_DIR/arena/hook_audit.py":/usr/local/lib/arena-hook-audit.py:ro \
      --entrypoint sh "$pixel_image" -c '
        pixel install >/dev/null 2>&1 || exit 1
        python3 /usr/local/lib/arena-hook-audit.py audit \
          --arm pixel --codex-home "$HOME/.codex" \
          --repo /tmp/preflight-repo --receipt /tmp/preflight.json
      ' >/dev/null 2>&1; then
    echo "ERROR: --review-pixel-hooks: the pixel image writes the native-default hook set (task-event only), which the paired audit rejects; this mode needs a pixel install that also writes the Codex prompt-submit and metrics hooks" >&2
    exit 2
  fi
fi

# --watch: open one Herdr pane per still-running container (inside Herdr,
# splitting the current pane right — side by side). Must happen before `docker
# wait`: after it, the containers have exited and `docker exec -it` fails with
# "container is not running". The tmux fallback stays after that wait —
# `docker logs -f` streams even for exited containers.
launch_watch_panes() {  # <container>...
  local watch_rep="$1"
  shift
  local c watch_c watch_pane watch_arm watch_owner watch_image watch_snap watch_out
  submit_codex_prompt() {
    local pane_id="$1" prompt="$2" attempt screen
    herdr pane send-text "$pane_id" "$prompt" >/dev/null 2>&1
    sleep 1
    for attempt in 1 2 3; do
      herdr pane send-keys "$pane_id" Enter >/dev/null 2>&1
      sleep 1
      screen=$(herdr pane read "$pane_id" 2>/dev/null | tail -20)
      case "$screen" in
        *"Working"*|*"Explored"*|*"•"*) return 0 ;;
      esac
    done
    echo "WARNING: Codex prompt was not visibly submitted in pane $pane_id" >&2
    return 1
  }
  watch_owner="${HERDR_PANE_ID:-}"
  if [ -z "$watch_owner" ]; then
    watch_owner=$(herdr pane list | python3 -c \
      "import json,sys; panes=json.load(sys.stdin)['result']['panes']; print(next((p['pane_id'] for p in panes if p.get('focused')), ''))" \
      2>/dev/null || true)
  fi
  if [ -z "$watch_owner" ]; then
    echo "WARNING: --watch could not identify the Herdr owner pane; set HERDR_PANE_ID" >&2
    return 1
  fi
  local ready=1
  for c in "$@"; do
    watch_arm=${c#arena-}; watch_arm=${watch_arm%%-*}
    watch_snap="$RESULTS/watch-snapshot-$watch_arm-$watch_rep"
    watch_out="$RESULTS/watch-out-$watch_arm-$watch_rep"
    watch_c="arena-watch-$watch_arm-$RUN_ID-$watch_rep"
    watch_image=$("$DOCKER_BIN" inspect -f '{{.Image}}' "$c")
    # A same-named leftover from an earlier run with this RUN_ID wedges
    # `create`; the arena-watch-* name belongs to this harness, so removing
    # the stale one first is safe.
    "$DOCKER_BIN" rm -f "$watch_c" >/dev/null 2>&1 || true
    if ! "$DOCKER_BIN" create --name "$watch_c" \
      -v "$watch_snap":/repo \
      -v "$AUTH":/root/.codex/auth.json:ro \
      -v "$ARENA_DIR/arena/entrypoint.sh":/usr/local/bin/arena-entrypoint:ro \
      -v "$ARENA_DIR/arena/context_manifest.py":/usr/local/lib/arena-context-manifest.py:ro \
      -v "$ARENA_DIR/arena/hook_audit.py":/usr/local/lib/arena-hook-audit.py:ro \
      -v "$ARENA_DIR/arena/skill_candidate.py":/usr/local/lib/arena-skill-candidate.py:ro \
      -v "$SCENARIOS_DIR":/prompts:ro \
      -v "$watch_out":/out \
      -e ARM_TOOL="$watch_arm" -e REP="$watch_rep" -e TASKS="$TASKS" -e ARENA_RUN_ID="$RUN_ID" \
      -e ARENA_WATCH_ONLY=1 -e ARENA_REVIEWED_PIXEL_HOOKS=0 \
      -e PIXEL_ARENA_PREP_GRAPH="$PREPARE_PIXEL_GRAPH" \
      -e PIXEL_ARENA_REUSE_INDEX="${PIXEL_ARENA_REUSE_INDEX:-1}" \
      -e ARENA_SKILL_PILOT="$SKILL_PILOT" \
      -e CODEX_MODEL="${CODEX_MODEL:-}" -e CODEX_EFFORT="${CODEX_EFFORT:-}" \
      "$watch_image" >/dev/null; then
      echo "WARNING: $watch_c create failed" >&2
      ready=0
      continue
    fi
    if ! "$DOCKER_BIN" start "$watch_c" >/dev/null; then
      echo "WARNING: $watch_c start failed" >&2
      "$DOCKER_BIN" rm -f "$watch_c" >/dev/null 2>&1 || true
      ready=0
      continue
    fi
    WATCH_CONTAINERS+=("$watch_c")
    for _ in $(seq 1 120); do
      if "$DOCKER_BIN" exec "$watch_c" test -e "/out/arena-ready-${watch_arm}-${watch_rep}" >/dev/null 2>&1; then
        break
      fi
      sleep 1
    done
    if ! "$DOCKER_BIN" exec "$watch_c" test -e "/out/arena-ready-${watch_arm}-${watch_rep}" >/dev/null 2>&1; then
      echo "WARNING: $watch_c did not finish setup before pane launch" >&2
      ready=0
    fi
  done
  [ "$ready" -eq 1 ] || return 1
  for c in "$@"; do
    watch_arm=${c#arena-}; watch_arm=${watch_arm%%-*}
    watch_c="arena-watch-$watch_arm-$RUN_ID-$watch_rep"
    watch_out="$RESULTS/watch-out-$watch_arm-$watch_rep"
    watch_pane=$(HERDR_PANE_ID="$watch_owner" herdr pane split --current --direction right \
      | python3 -c "import json,sys;print(json.load(sys.stdin)['result']['pane']['pane_id'])" 2>/dev/null || true)
    if [ -n "$watch_pane" ]; then
      echo "$watch_pane" >> "$RESULTS/.watch-panes"
      herdr pane rename "$watch_pane" "arena-$watch_arm" >/dev/null 2>&1
      # Interactive Codex runs in its isolated watch-only container. The task is sent
      # through the normal TUI after it reaches its input prompt.
      local prompt
      prompt=$(prompt_for "${TASKS%% *}")
      herdr pane run "$watch_pane" \
        "docker exec -it $watch_c codex -m $CODEX_MODEL -c model_reasoning_effort=$CODEX_EFFORT --dangerously-bypass-approvals-and-sandbox"
      # Codex may ask to trust /repo. Hook definitions require an explicit
      # human review in /hooks before the prompt is submitted.
      (
        pane_id="$watch_pane"
        for _ in $(seq 1 45); do
          screen=$(herdr pane read "$pane_id" 2>/dev/null | tail -20)
          case "$screen" in
            *"Hooks need review"*|*"Trust all and continue"*)
              echo "WARNING: review hooks in /hooks for pane $pane_id, then resume the pane" >&2
              herdr pane send-keys "$pane_id" esc >/dev/null 2>&1
              exit 1 ;;
            *"Trust and continue"*)
              herdr pane send-keys "$pane_id" Enter >/dev/null 2>&1 ;;
            *"Ask Codex"*)
              submit_codex_prompt "$pane_id" "$prompt" || true
              if [ "$watch_arm" = "pixel" ]; then
                for _ in $(seq 1 90); do
                  [ -s "$watch_out/pixel-hook-${watch_rep}.jsonl" ] && break
                  sleep 1
                done
              fi
              exit 0 ;;
          esac
          sleep 2
        done
      ) &
    else
      echo "WARNING: herdr pane split failed; $c logs via 'docker logs -f $c'" >&2
    fi
  done
  echo "watching: herdr panes running interactive codex ($# containers)"
}

launch_watch_tmux() {  # <container>... — tiled tmux fallback
  local c
  WATCH_SESSION="arena-$$"
  tmux new-session -d -s "$WATCH_SESSION" -x 220 -y 50 \
    "docker logs -f $1; echo; echo 'container exited'; exec \${SHELL:-sh}"
  for c in "${@:2}"; do
    tmux split-window -t "$WATCH_SESSION" \
      "docker logs -f $c; echo; echo 'container exited'; exec \${SHELL:-sh}"
    tmux select-layout -t "$WATCH_SESSION" tiled >/dev/null
  done
  tmux select-layout -t "$WATCH_SESSION" tiled >/dev/null
  echo "watching: tmux attach -t $WATCH_SESSION"
}

# all arms in parallel: one container each, all tasks inside
CONTAINERS=()
CONTAINER_ARMS=()
CONTAINER_REPS=()
WATCH_CONTAINERS=()
LAUNCH_FAIL=0
for rep in $(seq 1 "$REPS"); do
  # fresh snapshot per rep: prior reps' index artifacts and tool edits must
  # not leak into the next rep's starting state. Rep-scoped names —
  # deleting a shared snapshot while the previous rep's containers still
  # mount it races and kills their later tasks.
  for arm in $ARMS; do rm -rf "$RESULTS/snapshot-$arm-$rep" || true; done
  rep_containers=()
  arm_index=0
  for arm in $ARMS; do
    if ! launch_arm "$arm" "$rep" "${ARM_IMAGE_IDS[$arm_index]}"; then
      LAUNCH_FAIL=1
      for task in $TASKS; do touch "$RESULTS/$arm-$task-$rep.failed"; done
      arm_index=$((arm_index + 1))
      continue
    fi
    arm_index=$((arm_index + 1))
    CONTAINERS+=("arena-$arm-$RUN_ID-$rep")
    CONTAINER_ARMS+=("$arm")
    CONTAINER_REPS+=("$rep")
    rep_containers+=("arena-$arm-$RUN_ID-$rep")
  done
  # serialize reps: waiting here keeps rep N+1 from racing rep N's still
  # running containers for CPU — wall times stay comparable across reps
  if [ "${#rep_containers[@]}" -gt 0 ]; then
    # The Watch panes must open while the containers are still running: after
    # `docker wait` they have exited and `docker exec -it` cannot attach.
    if [ "$WATCH" = "1" ]; then
      if [ -n "${HERDR_ENV:-}" ] && command -v herdr >/dev/null 2>&1; then
        launch_watch_panes "$rep" "${rep_containers[@]}"
      elif command -v tmux >/dev/null 2>&1; then
        launch_watch_tmux "${rep_containers[@]}"
      else
        echo "WARNING: --watch needs herdr (HERDR_ENV) or tmux; neither found" >&2
      fi
    fi
    "$DOCKER_BIN" wait "${rep_containers[@]}" >/dev/null 2>&1 || true
  fi
done

FAIL="$LAUNCH_FAIL"
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
# Sweep watch containers that already exited; ones still running belong to
# panes the operator may still be using, so they are left for docker to reap.
for watch_c in "${WATCH_CONTAINERS[@]:-}"; do
  [ -n "$watch_c" ] && "$DOCKER_BIN" rm "$watch_c" >/dev/null 2>&1 || true
done
[ -z "$(find "$RESULTS" -maxdepth 1 \( -name 'pixel-hook-live-*.failed' -o -name 'pixel-hook-trust-*.failed' \) -print -quit 2>/dev/null)" ] || FAIL=1
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
    --codex-version "$CODEX_VERSION" --skill-candidate-name "$SKILL_NAME" \
    --skill-source-sha256 "$SKILL_SOURCE_SHA256"
}
if [ "$ASSERT_CONTEXT_PARITY" = "1" ]; then
  rank_results --assert-context-parity
elif [ "$SKILL_PILOT" = "1" ]; then
  rank_results --assert-skill-only "$SKILL_NAME"
else
  rank_results
fi
echo "total wall: $(( $(date +%s) - START ))s"
[ "$FAIL" -eq 0 ] || exit "$FAIL"
