#!/usr/bin/env bash
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

# eval/run.sh — repeatable harness trial: scenarios × arms × CLIs × repetitions.
#
# Arms:
#   baseline   no pixel: hooks stripped, AGENTS.md block stripped, no pixel
#              skill, no .pixel index, and (BASELINE_PIXEL=hidden, the default)
#              a `pixel` on PATH that answers "command not found"
#   quiet      the install, session-start doctrine only: `pixel install --repo`
#              in the scratch worktree + the deployed prompt, every hook
#              filtered down to `run-hook session-start` / `post-compaction`
#              (no prompt-submit task packet, metrics relay, guard, task events)
#   full       the install default: every pixel hook the operator's
#              `pixel install` wrote, plus `pixel install --repo` in the worktree
#   on         legacy: one clean pixel hook set + committed AGENTS.md block +
#              eval/variants/frozen-main payload
#   v<name>    legacy: payload from eval/variants/<name>/agent-prompt.md +
#              AGENTS.md block body from eval/variants/<name>/rules-body.md
#   quiet and full differ in the hook set alone; baseline and quiet differ in
#   the whole pixel integration (binary, index, doctrine, skill).
#
# Order: each (rep, cli, scenario) cell runs its arms in the counterbalanced
# order lib/arm_order.py derives from ORDER_SEED (the controlled runner's
# algorithm): over every len(ARMS) reps each arm holds each position once.
#
# Env:
#   CLIS=claude            claude|agy|codex|pi (codex/pi dispatch to eval/clis/)
#   ARMS="baseline on"     SUITE=<name>  (every scenario whose "suite" matches)
#   SCENARIOS="s1-hook-install ..."  (explicit list; wins over SUITE)
#   REPS=1                 ORDER_SEED=<SUITE, or "eval">
#   MAX_TURNS=12           per-scenario "max_turns" overrides (claude only)
#   RUN_TIMEOUT=1800       wall seconds per agent run
#   CLAUDE_MODEL=claude-opus-5-5   CODEX_MODEL=gpt-5.6-terra   CODEX_REASONING=medium
#   CLAUDE_BIN, CODEX_BIN, PIXEL_BIN   binaries (fixtures in the offline test)
#   RESULTS=eval/results   SCRATCH=/tmp/pixel-eval-run-$$   BASELINE_PIXEL=hidden|present
set -euo pipefail
unset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE   # caller exports would poison the scratch worktrees
EVAL_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO="$(git -C "$EVAL_DIR" rev-parse --show-toplevel)"
PIXEL_BIN="${PIXEL_BIN:-$HOME/.local/bin/pixel}"
# The host CLIs as PATH resolves them (an interactive alias never reaches this
# script), else the per-user install location.
CLAUDE_BIN="${CLAUDE_BIN:-$(command -v claude || echo "$HOME/.local/bin/claude")}"
export CODEX_BIN="${CODEX_BIN:-$(command -v codex || echo "$HOME/.local/bin/codex")}"
DEPLOY_PROMPT="$HOME/.local/share/pixel/agent-prompt.md"
RESULTS="${RESULTS:-$EVAL_DIR/results}"
SCEN_DIR="$EVAL_DIR/scenarios"
# Per-invocation scratch by default: concurrent runs must not share worktrees,
# configs, or the deployed-prompt swap. Override to reuse a warm scratch.
# Under /tmp on purpose: the operator's Codex config trusts /tmp, which
# project-scoped .codex/ files written by `pixel install --repo` need.
SCRATCH="${SCRATCH:-/tmp/pixel-eval-run-$$}"
# Every run changes into its scenario worktree: a relative RESULTS or SCRATCH
# would then name a path under that worktree, and the first transcript write
# fails ("No such file or directory") before the agent starts.
mkdir -p "$RESULTS" "$SCRATCH"
RESULTS="$(cd "$RESULTS" && pwd)"
SCRATCH="$(cd "$SCRATCH" && pwd)"
EVAL_HEAD="$(git -C "$EVAL_DIR" rev-parse HEAD)"
MAIN_ROOT="$(cd "$EVAL_DIR" && cd "$(git -C "$EVAL_DIR" rev-parse --git-common-dir)/.." && pwd)"
CLIS="${CLIS:-claude}"
ARMS="${ARMS:-baseline on}"
SUITE="${SUITE:-}"
if [ -z "${SCENARIOS:-}" ]; then
  if [ -n "$SUITE" ]; then
    SCENARIOS="$(python3 "$EVAL_DIR/lib/scenario.py" list "$SCEN_DIR" "$SUITE" | tr '\n' ' ')"
    [ -n "${SCENARIOS// /}" ] || { echo "suite $SUITE has no scenario in $SCEN_DIR" >&2; exit 2; }
  else
    SCENARIOS="s1-hook-install s2-vector-recall s3-rename-impact"
  fi
fi
REPS="${REPS:-1}"
ORDER_SEED="${ORDER_SEED:-${SUITE:-eval}}"
MAX_TURNS="${MAX_TURNS:-12}"
RUN_TIMEOUT="${RUN_TIMEOUT:-1800}"
export CLAUDE_MODEL="${CLAUDE_MODEL:-claude-opus-5-5}"
export CODEX_MODEL="${CODEX_MODEL:-gpt-5.6-terra}"
export CODEX_REASONING="${CODEX_REASONING:-medium}"
BASELINE_PIXEL="${BASELINE_PIXEL:-hidden}"
case "$BASELINE_PIXEL" in hidden|present) ;; *) echo "BASELINE_PIXEL must be hidden or present" >&2; exit 2 ;; esac
case "$REPS" in ''|*[!0-9]*|0) echo "REPS must be a positive integer" >&2; exit 2 ;; esac
mkdir -p "$RESULTS" "$SCRATCH"

now_ms() { python3 -c 'import time; print(int(time.time() * 1000))'; }
bounded() {  # seconds cmd... — coreutils timeout, gtimeout on macOS, else unbounded
  local secs="$1"; shift
  if command -v timeout >/dev/null 2>&1; then timeout "$secs" "$@"
  elif command -v gtimeout >/dev/null 2>&1; then gtimeout "$secs" "$@"
  else "$@"; fi
}
sget() { python3 "$EVAL_DIR/lib/scenario.py" get "$SCEN_DIR/$1.json" "$2" "${3:-}"; }
record() { python3 "$EVAL_DIR/lib/record.py" "$@"; }
sha16() { python3 -c 'import hashlib,os,sys; p=sys.argv[1]; print(hashlib.sha256(open(p,"rb").read()).hexdigest()[:16] if os.path.isfile(p) else "none")' "$1"; }

cli_version() {
  case "$1" in
    claude) "$CLAUDE_BIN" --version 2>/dev/null | head -1 ;;
    codex)  "$CODEX_BIN" --version 2>/dev/null | head -1 ;;
    agy)    agy --version 2>/dev/null | head -1 ;;
    pi)     "${PI_BIN:-pi}" --version 2>/dev/null | head -1 ;;
  esac
}
cli_model() {
  case "$1" in
    claude) echo "$CLAUDE_MODEL" ;;
    codex)  echo "$CODEX_MODEL/$CODEX_REASONING" ;;
    *)      echo default ;;
  esac
}
# A missing host binary fails every cell with rc 127 after its setup cost:
# refuse the campaign before the first one instead.
for cli in $CLIS; do
  case "$cli" in
    claude) bin="$CLAUDE_BIN" var=CLAUDE_BIN ;;
    codex)  bin="$CODEX_BIN" var=CODEX_BIN ;;
    *)      continue ;;
  esac
  if [ ! -x "$bin" ]; then
    echo "eval/run.sh: no $cli executable at $bin (set $var)" >&2
    exit 2
  fi
done
# Read once per campaign (bash 3.2 has no associative arrays: one variable
# per host, read back through version_of).
for cli in $CLIS; do
  v="$(cli_version "$cli" || true)"
  printf -v "CLI_VERSION_$cli" '%s' "${v:-unknown}"
done
version_of() { local var="CLI_VERSION_$1"; echo "${!var:-unknown}"; }
PIXEL_VERSION="$("$PIXEL_BIN" --version 2>/dev/null | head -1 || true)"

# Reused transcripts are only valid for the inputs that produced them; a
# mismatched results dir silently scores stale answers as current, which the
# gate cannot see. Refuse and let the operator archive instead.
versions=""; for cli in $CLIS; do versions="$versions $cli=$(version_of "$cli")/$(cli_model "$cli")"; done
RUN_IDENTITY="head=$EVAL_HEAD arms=$ARMS clis=$CLIS scenarios=$SCENARIOS turns=$MAX_TURNS reps=$REPS seed=$ORDER_SEED baseline_pixel=$BASELINE_PIXEL hosts:$versions"
IDENTITY_FILE="$RESULTS/.identity"
if [ -e "$IDENTITY_FILE" ] && [ "$(cat "$IDENTITY_FILE")" != "$RUN_IDENTITY" ]; then
  echo "results dir holds a different run:" >&2
  echo "  file:    $(cat "$IDENTITY_FILE")" >&2
  echo "  current: $RUN_IDENTITY" >&2
  echo "archive or empty $RESULTS, then re-run (or point RESULTS= at a fresh dir)" >&2
  exit 2
fi
printf '%s\n' "$RUN_IDENTITY" > "$IDENTITY_FILE"
CAMPAIGN="$RESULTS/campaign.json"
record "$CAMPAIGN" schema_version:=1 eval_head="$EVAL_HEAD" arms="$ARMS" clis="$CLIS" \
  suite="$SUITE" scenarios="$SCENARIOS" reps:="$REPS" order_seed="$ORDER_SEED" \
  order_algorithm="sha256-permutation-n-repetition-rotation-v1" max_turns:="$MAX_TURNS" \
  run_timeout_s:="$RUN_TIMEOUT" baseline_pixel="$BASELINE_PIXEL" pixel_version="$PIXEL_VERSION" \
  claude_model="$CLAUDE_MODEL" codex_model="$CODEX_MODEL" codex_reasoning="$CODEX_REASONING"
for cli in $CLIS; do record "$CAMPAIGN" "version_$cli=$(version_of "$cli")"; done

DEPLOY_BACKUP="$(mktemp -t pixel-eval-prompt.XXXXXX)"
AGY_BACKUP="$(mktemp -t pixel-eval-agy.XXXXXX)"
# `pixel install --repo` in a linked scratch worktree appends to the shared
# info/exclude; put the operator's file back byte for byte on exit.
EXCLUDE_FILE="$(git -C "$REPO" rev-parse --path-format=absolute --git-path info/exclude)"
EXCLUDE_BACKUP="$(mktemp -t pixel-eval-exclude.XXXXXX)"
EXCLUDE_EXISTED=0
[ -f "$EXCLUDE_FILE" ] && { cp "$EXCLUDE_FILE" "$EXCLUDE_BACKUP"; EXCLUDE_EXISTED=1; }
# The deployed-prompt swap is machine-global: serialize the whole campaign
# (backup → swaps → CLI runs → restore) against other campaigns and installs.
PROMPT_LOCK="$(dirname "$DEPLOY_PROMPT")/.eval-prompt.lock"
mkdir -p "$(dirname "$DEPLOY_PROMPT")"
acquire_prompt_lock() {
  local waited=0
  until mkdir "$PROMPT_LOCK" 2>/dev/null; do
    if [ -f "$PROMPT_LOCK/pid" ] && ! kill -0 "$(cat "$PROMPT_LOCK/pid" 2>/dev/null)" 2>/dev/null; then
      rm -rf "$PROMPT_LOCK"   # holder died without cleanup
      continue
    fi
    waited=$((waited + 5))
    if [ "$waited" -ge 600 ]; then
      echo "prompt lock still held after ${waited}s: $PROMPT_LOCK (remove it if no campaign is running)" >&2
      exit 3
    fi
    sleep 5
  done
  echo $$ > "$PROMPT_LOCK/pid"
}
release_prompt_lock() { rm -rf "$PROMPT_LOCK" 2>/dev/null || true; }
acquire_prompt_lock
[ -f "$DEPLOY_PROMPT" ] && cp "$DEPLOY_PROMPT" "$DEPLOY_BACKUP"
restore() {
  release_prompt_lock
  if [ -s "$DEPLOY_BACKUP" ] && ! cmp -s "$DEPLOY_BACKUP" "$DEPLOY_PROMPT"; then
    cp "$DEPLOY_BACKUP" "$DEPLOY_PROMPT" 2>/dev/null || true
  fi
  rm -f "$DEPLOY_BACKUP"
  if [ "$EXCLUDE_EXISTED" = 1 ] && ! cmp -s "$EXCLUDE_BACKUP" "$EXCLUDE_FILE"; then
    cp "$EXCLUDE_BACKUP" "$EXCLUDE_FILE" 2>/dev/null || true
  elif [ "$EXCLUDE_EXISTED" = 0 ]; then
    rm -f "$EXCLUDE_FILE"
  fi
  rm -f "$EXCLUDE_BACKUP"
  if [ -s "$AGY_BACKUP" ]; then
    while read -r name state; do
      if [ "$state" = disabled ]; then agy plugin disable "$name" >/dev/null 2>&1 || true
      else agy plugin enable "$name" >/dev/null 2>&1 || true; fi
    done < "$AGY_BACKUP"
  fi
  rm -f "$AGY_BACKUP"
}
trap restore EXIT

log() { printf '\n=== %s ===\n' "$*"; }

# --- sandbox builders -------------------------------------------------------
strip_pixel_hooks_py() { python3 "$EVAL_DIR/lib/strip_pixel_hooks.py"; }
filter_hooks_py() { python3 "$EVAL_DIR/lib/filter_hooks.py" "$1"; }
merge_single_hook_set_py() {
  local mode="quiet"
  case "$1" in on|vslim|vminimal) mode="full" ;; esac
  python3 "$EVAL_DIR/lib/merge_single_hook_set.py" "$PIXEL_BIN" "$mode"
}
apply_agents_block_py() { python3 "$EVAL_DIR/lib/apply_agents_block.py" "$@"; }
hook_mode() { case "$1" in baseline) echo none ;; quiet) echo quiet ;; full) echo full ;; *) echo legacy ;; esac; }

# A `pixel` that is not there: the baseline agent sees what a machine
# without pixel shows, and the attempt still lands in its transcript.
NOPIXEL="$SCRATCH/nopixel"
mkdir -p "$NOPIXEL"
for name in pixel pixel-dev; do
  printf '#!/bin/sh\necho "%s: command not found" >&2\nexit 127\n' "$name" > "$NOPIXEL/$name"
  chmod +x "$NOPIXEL/$name"
done
path_for() {  # arm -> PATH the agent runs with
  if [ "$1" = baseline ] && [ "$BASELINE_PIXEL" = hidden ]; then echo "$NOPIXEL:$PATH"; else echo "$PATH"; fi
}

# The scenarios and heldout checks are the answers: keep them out of the
# tree the agent explores (skip-worktree keeps `git status` clean).
hide_harness() {
  local wt="$1"
  git -C "$wt" ls-files -z -- eval | xargs -0 git -C "$wt" update-index --skip-worktree --
  rm -rf "$wt/eval"
}

# One frozen tree + index per pinned commit: the cold setup is paid once,
# measured, and kept out of every run's wall time.
template_for() {  # commit -> template dir (built once)
  local commit="$1" tpl="$SCRATCH/tpl-${1:0:12}"
  if [ ! -d "$tpl" ]; then
    git -C "$REPO" worktree add --detach "$tpl" "$commit" >/dev/null 2>&1
    hide_harness "$tpl"
    local t0 t1 t2; t0=$(now_ms)
    "$PIXEL_BIN" build-index --history "$tpl" > "$tpl.setup.log" 2>&1 || echo "build-index failed for $commit (see $tpl.setup.log)" >&2
    t1=$(now_ms)
    "$PIXEL_BIN" prepare-repo --no-daemon "$tpl" >> "$tpl.setup.log" 2>&1 || echo "prepare-repo failed for $commit (see $tpl.setup.log)" >&2
    t2=$(now_ms)
    ( cd "$tpl" && "$PIXEL_BIN" daemon stop "$tpl" >/dev/null 2>&1 ) || true
    rm -f "$tpl/.pixel/actions.jsonl"
    printf '{"commit":"%s","build_index_ms":%s,"prepare_repo_ms":%s,"pixel_bytes":%s,"pixel_version":"%s"}\n' \
      "$commit" "$((t1 - t0))" "$((t2 - t1))" "$(du -sk "$tpl/.pixel" 2>/dev/null | cut -f1 | awk '{print $1*1024}')" \
      "$PIXEL_VERSION" >> "$RESULTS/setup.jsonl"
  fi
  echo "$tpl"
}

build_arm() {  # arm commit
  local arm="$1" commit="$2" wt="$SCRATCH/wt-$1" cfg="$SCRATCH/cfg-$1"
  local tpl; tpl="$(template_for "$commit")"
  if [ ! -d "$wt" ]; then
    git -C "$REPO" worktree add --detach "$wt" "$commit" >/dev/null 2>&1
  fi
  # Every run starts from the pinned tree: an edit task's changes, a build's
  # output and the previous run's .pixel must not reach the next one.
  git -C "$wt" checkout -q --detach -f "$commit"
  git -C "$wt" reset -q --hard "$commit"
  git -C "$wt" clean -ffdxq
  hide_harness "$wt"
  if [ "$arm" != baseline ] || [ "$BASELINE_PIXEL" = present ]; then
    cp -R "$tpl/.pixel" "$wt/.pixel"
  fi
  case "$arm" in
    baseline)
      (cd "$wt" && apply_agents_block_py strip)
      rm -rf "$wt/.agents/skills/pixel"
      ;;
    quiet|full)
      "$PIXEL_BIN" install --repo "$wt" > "$SCRATCH/install-$arm.log" 2>&1 \
        || { echo "pixel install --repo failed for arm $arm (see $SCRATCH/install-$arm.log)" >&2; exit 2; }
      # Its backups would hand the agent a second copy of AGENTS.md.
      rm -f "$wt"/AGENTS.md.pixel-bak.* "$wt"/.codex/*.pixel-bak.* "$wt/.pixel/actions.jsonl"
      if [ "$arm" = quiet ]; then
        for f in "$wt/.claude/settings.local.json" "$wt/.codex/hooks.json"; do
          [ -f "$f" ] && filter_hooks_py quiet < "$f" > "$f.tmp" && mv "$f.tmp" "$f"
        done
      fi
      ;;
    on)
      (cd "$wt" && apply_agents_block_py replace "$EVAL_DIR/variants/frozen-main/rules-body.md")
      ;;
    vquiet)
      (cd "$wt" && apply_agents_block_py replace "$EVAL_DIR/variants/slim/rules-body.md")
      ;;
    v*)
      local body="$EVAL_DIR/variants/${arm#v}/rules-body.md"
      local payload="$EVAL_DIR/variants/${arm#v}/agent-prompt.md"
      [ -f "$body" ] && [ -f "$payload" ] || { echo "variant ${arm#v} missing files" >&2; exit 2; }
      (cd "$wt" && apply_agents_block_py replace "$body")
      ;;
  esac
  # claude config: real settings with the arm's pixel hook set — baseline
  # none, quiet/full filtered from the installed set, legacy arms one clean set
  rm -rf "$cfg"; mkdir -p "$cfg"
  # The isolated config still needs the operator's login: without it every
  # `claude -p` answers "Not logged in" and spends nothing.
  if [ -f "$HOME/.claude/.credentials.json" ]; then
    cp "$HOME/.claude/.credentials.json" "$cfg/.credentials.json"
    chmod 600 "$cfg/.credentials.json"
  fi
  local mode; mode="$(hook_mode "$arm")"
  if [ "$mode" = legacy ]; then
    strip_pixel_hooks_py < "$HOME/.claude/settings.json" > "$cfg/settings.json"
    merge_single_hook_set_py "$arm" < "$cfg/settings.json" > "$cfg/settings.json.tmp" && mv "$cfg/settings.json.tmp" "$cfg/settings.json"
  elif [ "$mode" = none ]; then
    strip_pixel_hooks_py < "$HOME/.claude/settings.json" > "$cfg/settings.json"
  else
    filter_hooks_py "$mode" < "$HOME/.claude/settings.json" > "$cfg/settings.json"
    grep -q 'run-hook session-start' "$cfg/settings.json" \
      || { echo "arm $arm: ~/.claude/settings.json has no pixel session-start hook — run \`pixel install\` first" >&2; exit 2; }
  fi
  [ -f "$HOME/.claude/CLAUDE.md" ] && cp "$HOME/.claude/CLAUDE.md" "$cfg/CLAUDE.md" || true
  for name in plugins agents commands context-mode advanced-memory; do
    [ -e "$HOME/.claude/$name" ] && [ ! -e "$cfg/$name" ] && ln -s "$HOME/.claude/$name" "$cfg/$name" || true
  done
  # Skills one by one: the baseline must not load a pixel skill.
  if [ -d "$HOME/.claude/skills" ]; then
    mkdir -p "$cfg/skills"
    for skill in "$HOME/.claude/skills"/*; do
      [ -e "$skill" ] || continue
      case "$(basename "$skill")" in pixel*) [ "$arm" = baseline ] && continue ;; esac
      ln -s "$skill" "$cfg/skills/$(basename "$skill")"
    done
  fi
  # codex config: per-arm CODEX_HOME with the payload channel (baseline strips
  # the pixel-managed block, variants swap its content, quiet/full keep the
  # installed one) plus login state. Built only when codex is in the run:
  # machines without ~/.codex must be able to evaluate the other CLIs.
  local codex_home="$cfg/codex-home"
  if [[ "$CLIS" != *codex* ]]; then
    echo "built arm=$arm commit=${commit:0:12} wt=$wt cfg=$cfg (no codex config: CLIS=$CLIS)"
    return
  fi
  mkdir -p "$codex_home"
  local variant=""
  case "$arm" in
    baseline|quiet|full) variant="" ;;
    on) variant="frozen-main" ;;
    vquiet|vslim) variant="slim" ;;
    vfinal) variant="final" ;;
    vminimal) variant="minimal" ;;
    v*) variant="${arm#v}" ;;
  esac
  if [ "$arm" = quiet ] || [ "$arm" = full ]; then
    python3 "$EVAL_DIR/lib/build_codex_cfg.py" "$HOME/.codex/config.toml" "$codex_home/config.toml" keep
    if [ -f "$HOME/.codex/hooks.json" ]; then
      filter_hooks_py "$mode" < "$HOME/.codex/hooks.json" > "$codex_home/hooks.json"
    fi
  elif [ -n "$variant" ]; then
    local codex_payload="$EVAL_DIR/variants/$variant/agent-prompt.md"
    [ -f "$codex_payload" ] || { echo "no payload for variant $variant (arm $arm)" >&2; exit 2; }
    python3 "$EVAL_DIR/lib/build_codex_cfg.py" "$HOME/.codex/config.toml" "$codex_home/config.toml" payload "$codex_payload"
  else
    python3 "$EVAL_DIR/lib/build_codex_cfg.py" "$HOME/.codex/config.toml" "$codex_home/config.toml" baseline
  fi
  [ -f "$HOME/.codex/auth.json" ] && cp "$HOME/.codex/auth.json" "$codex_home/" || true
  echo "built arm=$arm commit=${commit:0:12} wt=$wt cfg=$cfg"
}

# --- CLI runners ------------------------------------------------------------
run_cli() {  # cli arm scenario outfile turns
  local cli="$1" arm="$2" scenario="$3" out="$4" turns="$5"
  local wt="$SCRATCH/wt-$arm" cfg="$SCRATCH/cfg-$arm"
  local prompt; prompt="$(sget "$scenario" prompt)"
  local agent_path; agent_path="$(path_for "$arm")"
  case "$cli" in
    claude)
      (cd "$wt" && PATH="$agent_path" CLAUDE_CONFIG_DIR="$cfg" bounded "$RUN_TIMEOUT" "$CLAUDE_BIN" -p "$prompt" \
        --model "$CLAUDE_MODEL" --output-format stream-json --verbose --max-turns "$turns" \
        --dangerously-skip-permissions > "$out" 2> "${out%.jsonl}.err")
      ;;
    agy)
      (cd "$wt" && PATH="$agent_path" agy -p "$prompt" --output-format stream-json --print-timeout 420s \
        > "$out" 2> "${out%.jsonl}.err")
      ;;
    codex|pi)
      if [ -x "$EVAL_DIR/clis/$cli.sh" ]; then
        ( PATH="$agent_path" WT="$wt" CFG="$cfg" PROMPT="$prompt" OUT="$out" MAX_TURNS="$turns" \
          bounded "$RUN_TIMEOUT" "$EVAL_DIR/clis/$cli.sh" )
      else
        echo "run_cli: $cli needs an executable eval/clis/$cli.sh reading \$WT \$CFG \$PROMPT \$OUT (see eval/README.md)" >&2
        return 9
      fi ;;
  esac
}

swap_payload() {  # arm -> deploy the payload that arm's session-start hook should emit
  local arm="$1"
  case "$arm" in
    on) cp "$EVAL_DIR/variants/frozen-main/agent-prompt.md" "$DEPLOY_PROMPT" ;;
    vfinal) cp "$EVAL_DIR/variants/final/agent-prompt.md" "$DEPLOY_PROMPT" ;;
    vquiet|vslim) cp "$EVAL_DIR/variants/slim/agent-prompt.md" "$DEPLOY_PROMPT" ;;
    vminimal) cp "$EVAL_DIR/variants/minimal/agent-prompt.md" "$DEPLOY_PROMPT" ;;
    v*) cp "$EVAL_DIR/variants/${arm#v}/agent-prompt.md" "$DEPLOY_PROMPT" ;;
    # The installed prompt, whatever an earlier arm of the rotation swapped in.
    quiet|full) if [ -s "$DEPLOY_BACKUP" ] && ! cmp -s "$DEPLOY_BACKUP" "$DEPLOY_PROMPT"; then cp "$DEPLOY_BACKUP" "$DEPLOY_PROMPT"; fi ;;
    baseline) : ;;   # no hooks run, payload irrelevant
  esac
}
payload_of() {  # arm -> sha16 of the payload its hooks emit ("none" for baseline)
  if [ "$1" = baseline ]; then echo none; else sha16 "$DEPLOY_PROMPT"; fi
}

agy_plugin_guard() {  # keep agy's global pixel plugin only for the `on` arm
  if [ "$CLIS" != claude ] && [[ "$CLIS" == *agy* ]]; then
    # Capture via tmp+mv: a failed `agy plugin list` must not leave an empty
    # file shadowing every later capture attempt (restore reads it with -s).
    if [ ! -s "$AGY_BACKUP" ]; then
      if agy plugin list 2>/dev/null | python3 -c '
import json,sys
try: d=json.load(sys.stdin)
except Exception: d={}
for i in d.get("imports",[]):
    if i.get("name")=="pixel": print("pixel", "enabled")' > "$AGY_BACKUP.tmp" && [ -s "$AGY_BACKUP.tmp" ]; then
        mv "$AGY_BACKUP.tmp" "$AGY_BACKUP"
      else
        rm -f "$AGY_BACKUP.tmp"
      fi
    fi
    if [ "$1" = off ]; then agy plugin disable pixel >/dev/null 2>&1 || true
    else agy plugin enable pixel >/dev/null 2>&1 || true; fi
  fi
}

run_verifier() {  # scenario wt out target -> writes <out>.verify.json
  local scenario="$1" wt="$2" out="$3" target="$4"
  local script tmo hd rc=0 t0 t1
  script="$(sget "$scenario" verifier.script)"
  tmo="$(sget "$scenario" verifier.timeout_s 900)"
  hd="$SCRATCH/heldout-$scenario"
  rm -rf "$hd"; cp -R "$EVAL_DIR/heldout/$scenario" "$hd"
  t0=$(now_ms)
  (cd "$wt" && HELDOUT="$hd" WT="$wt" CARGO_TARGET_DIR="$target" bounded "$tmo" sh "$hd/$script") \
    > "${out%.jsonl}.verify.log" 2>&1 || rc=$?
  t1=$(now_ms)
  rm -f "${out%.jsonl}.verify.json"
  record "${out%.jsonl}.verify.json" passed:="$([ "$rc" = 0 ] && echo true || echo false)" rc:="$rc" \
    timed_out:="$([ "$rc" = 124 ] && echo true || echo false)" wall_ms:="$((t1 - t0))" \
    script="$script" heldout_sha256@="$EVAL_DIR/heldout/$scenario"
  rm -rf "$hd"
}

run_one() {  # rep scenario cli arm position
  local rep="$1" scenario="$2" cli="$3" arm="$4" pos="$5"
  local dir="$RESULTS/rep-$rep"; mkdir -p "$dir"
  local out="$dir/$scenario-$arm.$cli.jsonl"
  local meta="$out.meta" run="${out%.jsonl}.run.json"
  local commit mode turns warm target
  commit="$(sget "$scenario" commit "$EVAL_HEAD")"
  mode="$(sget "$scenario" mode answer)"
  turns="$(sget "$scenario" max_turns "$MAX_TURNS")"
  warm="$(sget "$scenario" warm)"
  swap_payload "$arm"
  agy_plugin_guard "$([ "$arm" = on ] && echo on || echo off)"
  local identity="head=$EVAL_HEAD commit=$commit arm=$arm cli=$cli rep=$rep model=$(cli_model "$cli") version=$(version_of "$cli") payload=$(payload_of "$arm") turns=$turns"
  if [ -s "$out" ]; then
    if [ -f "$meta" ] && [ "$(cat "$meta")" = "$identity" ] && [ -f "$run" ]; then
      log "skip rep=$rep $scenario/$arm/$cli (exists, same identity)"
      return 0
    fi
    log "stale rep=$rep $scenario/$arm/$cli (identity changed) — archiving and rerunning"
    mkdir -p "$RESULTS/stale"
    mv "$out" "$RESULTS/stale/rep-$rep.$scenario-$arm.$cli.$(date +%s).jsonl.old"
  fi
  rm -f "$run" "${out%.jsonl}.verify.json"
  printf '%s\n' "$identity" > "$meta"
  build_arm "$arm" "$commit"
  local wt="$SCRATCH/wt-$arm" cfg="$SCRATCH/cfg-$arm"
  target="$SCRATCH/target-$arm-${commit:0:12}"
  export CARGO_TARGET_DIR="$target"
  local warm_ms=0
  if [ -n "$warm" ]; then
    local w0; w0=$(now_ms)
    (cd "$wt" && sh -c "$warm") > "${out%.jsonl}.warm.log" 2>&1 || echo "warm failed (see ${out%.jsonl}.warm.log)" >&2
    warm_ms=$(( $(now_ms) - w0 ))
  fi
  log "run  rep=$rep scenario=$scenario arm=$arm cli=$cli position=$pos"
  local t0 t1 rc=0
  t0=$(now_ms)
  run_cli "$cli" "$arm" "$scenario" "$out" "$turns" || rc=$?
  t1=$(now_ms)
  [ "$rc" = 0 ] || echo "run failed rc=$rc ($scenario/$arm/$cli rep $rep)"
  # A host that never reached the model (no login, an API error before the
  # first turn) would be scored as a failed answer and read as a tie or a
  # loss. Stop the campaign before recording the cell, so a rerun repeats it.
  # lib/host_reached.py reads the host's status events and stderr, never the
  # answer text, so an answer quoting a login error is still recorded.
  if ! python3 "$EVAL_DIR/lib/host_reached.py" "$out" "${out%.jsonl}.err"; then
    echo "eval/run.sh: $cli never reached the model in $scenario/$arm rep $rep (see $out); stopping the campaign" >&2
    rm -f "$meta"
    exit 3
  fi
  # Pixel's own action log is a second witness of what the agent ran.
  [ -f "$wt/.pixel/actions.jsonl" ] && cp "$wt/.pixel/actions.jsonl" "${out%.jsonl}.actions.jsonl" || true
  if [ "$mode" = edit ]; then
    run_verifier "$scenario" "$wt" "$out" "$target"
  fi
  local changed; changed="$(git -C "$wt" status --porcelain | wc -l | tr -d ' ')"
  record "$run" schema_version:=1 scenario="$scenario" arm="$arm" cli="$cli" rep:="$rep" \
    position:="$pos" order_seed="$ORDER_SEED" commit="$commit" eval_head="$EVAL_HEAD" mode="$mode" \
    model="$(cli_model "$cli")" cli_version="$(version_of "$cli")" pixel_version="$PIXEL_VERSION" \
    payload="$(payload_of "$arm")" baseline_pixel="$BASELINE_PIXEL" max_turns:="$turns" \
    exit_code:="$rc" timed_out:="$([ "$rc" = 124 ] && echo true || echo false)" \
    wall_ms:="$((t1 - t0))" warm_ms:="$warm_ms" started_ms:="$t0" files_changed:="$changed" \
    claude_settings@="$cfg/settings.json" codex_hooks@="$cfg/codex-home/hooks.json" \
    repo_claude_hooks@="$wt/.claude/settings.local.json" repo_codex_hooks@="$wt/.codex/hooks.json"
  if [ "$arm" != baseline ] || [ "$BASELINE_PIXEL" = present ]; then
    "$PIXEL_BIN" daemon stop "$wt" >/dev/null 2>&1 || true
  fi
}

# --- main loop --------------------------------------------------------------
for rep in $(seq 1 "$REPS"); do
  for scenario in $SCENARIOS; do
    [ -f "$SCEN_DIR/$scenario.json" ] || { echo "no scenario $scenario in $SCEN_DIR" >&2; exit 2; }
    for cli in $CLIS; do
      pos=0
      for arm in $(python3 "$EVAL_DIR/lib/arm_order.py" "$ORDER_SEED" "$((rep - 1))" "$cli/$scenario" $ARMS); do
        pos=$((pos + 1))
        run_one "$rep" "$scenario" "$cli" "$arm" "$pos"
      done
    done
  done
done

log "scoring"
# No arm filter: scores.json must always carry every arm's rows on disk, so a
# candidate-only invocation cannot wipe the baseline rows the gate needs.
python3 "$EVAL_DIR/score.py" --results "$RESULTS" --scenarios-dir "$SCEN_DIR"
# The scratch stays for a resumed campaign (warm cargo caches, built indexes).
echo
echo "scratch kept for a resume: $SCRATCH (worktrees of $REPO). When done:"
echo "  rm -rf '$SCRATCH' && git -C '$REPO' worktree prune"
