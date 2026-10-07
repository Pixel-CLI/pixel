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
#   --watch opens one Herdr pane per arm container (when inside Herdr)
#   streaming the measured agent output; falls back to a tmux session otherwise.
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


# --- --watch helpers (defined here; invoked per rep-1 container at launch) ---
# The pane follows the measured Codex task output; it never starts a second
# agent or submits another prompt against the measured repository.
open_watch_pane() {  # container arm
  local c="$1" arm="$2" wp task log_path monitor_cmd
  wp=$(herdr pane split --current --direction right --no-focus \
    | python3 -c "import json,sys;print(json.load(sys.stdin)['result']['pane']['pane_id'])" 2>/dev/null || true)
  if [ -z "$wp" ]; then
    echo "WARNING: herdr pane split failed; $c logs via 'docker logs -f $c'" >&2
    return 0
  fi
  echo "$wp" >> "$RESULTS/.watch-panes"
  herdr pane rename "$wp" "arena-$RUN_ID-$arm" >/dev/null 2>&1
  task="${TASKS%% *}"
  log_path="/out/${arm}-${task}-1.jsonl"
  printf -v monitor_cmd 'docker exec -it %q sh -c %q sh %q' "$c" \
    'while [ ! -f "$1" ]; do sleep 1; done; tail -n +1 -f "$1"' "$log_path"
  herdr pane run "$wp" "$monitor_cmd" >/dev/null 2>&1
}

# Equalize all panes in the tab to identical column widths: probe the layout
# with a zero-delta resize (returns geometry, moves nothing), then drag each
# column boundary to i/N of the row width until every column matches.
equalize_watch_panes() {
  python3 - <<'PYEQ' 2>/dev/null || true
import json, subprocess, time

def herdr(*a):
    try:
        return json.loads(subprocess.run(
            ["herdr", *a], capture_output=True, text=True, timeout=15).stdout)
    except Exception:
        return None

def layout(pane):
    r = herdr("pane", "resize", "--pane", pane, "--direction", "left",
              "--amount", "0")
    try:
        return r["result"]["resize"]["layout"]
    except (TypeError, KeyError):
        return {}

panes = herdr("pane", "list")
anchor = next((p["pane_id"] for p in (panes or {}).get("result", {}).get("panes", [])), "")
for _ in range(12):
    lay = layout(anchor)
    cols = sorted(lay.get("panes", []), key=lambda p: p["rect"]["x"])
    if len(cols) < 2:
        break
    width = lay["area"]["width"]
    moved = False
    for k in range(len(cols) - 1):
        gap = cols[k + 1]["rect"]["x"] - round(width * (k + 1) / len(cols))
        if abs(gap) <= 1:
            continue
        herdr("pane", "resize", "--pane", cols[k + 1]["pane_id"],
              "--direction", "left" if gap > 0 else "right",
              "--amount", str(round(abs(gap) / width, 3)))
        moved = True
        break
    if not moved:
        break
    time.sleep(0.1)
PYEQ
}

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
  # panes open the moment the container exists — never wait for reps to finish
  if [ "$rep" = "1" ] && [ "$WATCH" = "1" ] && [ -n "${HERDR_ENV:-}" ] \
    && command -v herdr >/dev/null 2>&1; then
    open_watch_pane "$container" "$arm"
  fi
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

# all arms in parallel: one container each, all tasks inside
CONTAINERS=()
CONTAINER_ARMS=()
CONTAINER_REPS=()
LAUNCH_FAIL=0
if [ "$WATCH" = "1" ] && [ -n "${HERDR_ENV:-}" ] && command -v herdr >/dev/null 2>&1; then
  # close prior arena/ab panes first — even ones this run did not create —
  # then each new rep-1 container opens its own pane as soon as it starts
  if [ -f "$RESULTS/.watch-panes" ]; then
    while IFS= read -r old_pane; do
      [ -n "$old_pane" ] && herdr pane close "$old_pane" >/dev/null 2>&1
    done < "$RESULTS/.watch-panes"
  fi
  for stale_pane in $(herdr pane list 2>/dev/null | python3 -c \
    "import json,sys
run_prefix = 'arena-' + sys.argv[1] + '-'
for p in json.load(sys.stdin)['result']['panes']:
    label = p.get('label') or ''
    if label.startswith(run_prefix):
        print(p['pane_id'])" "$RUN_ID" 2>/dev/null); do
    herdr pane close "$stale_pane" >/dev/null 2>&1
  done
fi

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
  if [ "$rep" = "1" ] && [ "$WATCH" = "1" ] && [ -n "${HERDR_ENV:-}" ]; then
    # all rep-1 panes exist — flatten to equal columns and label the tab
    # immediately, before the serialized wait
    equalize_watch_panes
    herdr tab rename "$(herdr pane list 2>/dev/null | python3 -c \
      "import json,sys;print(json.load(sys.stdin)['result']['panes'][0]['tab_id'])" 2>/dev/null)" \
      "arena: $(echo "$ARMS" | tr ' ' '|')" >/dev/null 2>&1 || true
  fi
  # serialize reps: waiting here keeps rep N+1 from racing rep N's still
  # running containers for CPU — wall times stay comparable across reps
  if [ "${#rep_containers[@]}" -gt 0 ]; then
    "$DOCKER_BIN" wait "${rep_containers[@]}" >/dev/null 2>&1 || true
  fi
done

# --watch: one pane per container streaming `docker logs -f`. Inside Herdr
# (HERDR_ENV=1) splits the current pane right, side by side with this one;
# otherwise falls back to a tiled tmux session you attach separately.
if [ "$WATCH" = "1" ]; then
  if [ -n "${HERDR_ENV:-}" ] && command -v herdr >/dev/null 2>&1; then
    echo "watching: herdr panes opened at launch (${#CONTAINERS[@]} containers)"
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
