#!/usr/bin/env bash
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

# Run one case of the arena catalog (SKILL.md) in two Codex panes side by side
# in Herdr: `arena:<case>:pixel` (Pixel's prompt-submit hook only) and
# `arena:<case>:no-pixel` (no hook, PIXEL_BRIEF=0). Each arm has its own clone
# of the repository and its own CODEX_HOME. See SKILL.md.
set -euo pipefail

SELF_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
SKILL_MD="$SELF_DIR/SKILL.md"
SCRATCH=${ARENA_SCRATCH:-/private/tmp/claude-501/arena}
CODEX_BIN=${ARENA_CODEX_BIN:-/Users/livio/.local/bin/codex}
USER_CODEX_HOME=${ARENA_USER_CODEX_HOME:-$HOME/.codex}
PREFIX=arena

usage() {
  cat <<'EOF'
Usage: arena.sh <command> [args]

  list                              the 8-case catalog: number, name, prompt
  run <n|name> [options]            run one case in two Codex panes
      --pixel-bin PATH              Pixel binary for the pixel arm (default: pixel on PATH)
      --sha COMMIT                  commit both arms are cloned at (default: origin/main)
      --timeout SECONDS             idle wait per arm (default: 600)
      --focus                       focus the arena tab when it opens
  status                            arena panes and their agent state
  receipt <run>                     rebuild and print a run's receipt
  abort [<case>]                    interrupt the agents, close the arena panes,
                                    delete the copied login of those runs
EOF
}

die() { printf 'arena: %s\n' "$*" >&2; exit 1; }
say() { printf 'arena: %s\n' "$*" >&2; }
hd() { HERDR_ENV=1 herdr "$@"; }

# ---------------------------------------------------------------- catalog

# TSV: number, name, prompt. SKILL.md holds the one copy of the catalog.
catalog() {
  awk '
    /^[0-9]+\. `[a-z]+` / {
      n = $1; sub(/\.$/, "", n)
      name = $2; gsub(/`/, "", name)
      want = 1; next
    }
    want && /^   `.*`$/ {
      p = $0; sub(/^   `/, "", p); sub(/`$/, "", p)
      printf "%s\t%s\t%s\n", n, name, p; want = 0
    }
  ' "$SKILL_MD"
}

cmd_list() {
  catalog | awk -F'\t' '{ printf "%s  %-13s %s\n", $1, $2, $3 }'
}

# ---------------------------------------------------------------- workspace

sha256_of() { shasum -a 256 "$1" | awk '{print $1}'; }

# Build (or reuse) the two clones of $SHA. Sets KEY, KEY_DIR, PIXEL_WS,
# NOPIXEL_WS, PIXEL_COPY.
prepare() {
  local bin=$1 sha_arg=$2 src full bin_hash
  src=$(git -C "$SELF_DIR" rev-parse --git-common-dir) || die 'run from a pixel checkout'
  src=$(cd "$SELF_DIR" && cd "$src" && pwd)
  if [ -z "$sha_arg" ]; then
    git -C "$src" fetch -q origin main || say 'fetch origin main failed; using the local ref'
    sha_arg=origin/main
  fi
  full=$(git -C "$src" rev-parse --verify "$sha_arg^{commit}") || die "unknown commit: $sha_arg"
  bin_hash=$(sha256_of "$bin")
  KEY="${full:0:12}-${bin_hash:0:12}"
  KEY_DIR="$SCRATCH/ws/$KEY"
  PIXEL_WS="$KEY_DIR/pixel/ws"
  NOPIXEL_WS="$KEY_DIR/no-pixel/ws"
  PIXEL_COPY="$KEY_DIR/bin/pixel"
  SHA_FULL=$full
  BIN_HASH=$bin_hash
  mkdir -p "$KEY_DIR/bin"
  if [ ! -x "$PIXEL_COPY" ] || [ "$(sha256_of "$PIXEL_COPY")" != "$bin_hash" ]; then
    cp "$bin" "$PIXEL_COPY.tmp" && chmod +x "$PIXEL_COPY.tmp" && mv -f "$PIXEL_COPY.tmp" "$PIXEL_COPY"
  fi
  local arm ws
  for arm in pixel no-pixel; do
    ws="$KEY_DIR/$arm/ws"
    if [ ! -f "$ws/.arena-ready" ]; then
      say "cloning $arm workspace at ${full:0:12}"
      rm -rf "${KEY_DIR:?}/$arm"
      mkdir -p "$KEY_DIR/$arm"
      git clone -q --no-checkout "$src" "$ws"
      git -C "$ws" checkout -q --detach "$full"
      git -C "$ws" remote remove origin
      touch "$ws/.arena-ready"
    fi
  done
  if [ ! -f "$KEY_DIR/pixel/indexed" ]; then
    say 'indexing the pixel workspace (prepare-repo, history)'
    (cd "$PIXEL_WS" && "$PIXEL_COPY" prepare-repo . >"$KEY_DIR/pixel/prepare.log" 2>&1 \
      && "$PIXEL_COPY" build-index --history . >>"$KEY_DIR/pixel/prepare.log" 2>&1) \
      || die "indexing failed; see $KEY_DIR/pixel/prepare.log"
    touch "$KEY_DIR/pixel/indexed"
  fi
  # Warm the daemon (a cheap no-model hook call) and prove the no-pixel clone
  # carries no Pixel state at all.
  hook_payload "$PIXEL_WS" "warm" | "$PIXEL_COPY" run-hook task-event --provider codex --event prompt-submit >/dev/null 2>&1 || true
  [ ! -e "$NOPIXEL_WS/.pixel" ] || die "no-pixel workspace has .pixel/ state: $NOPIXEL_WS"
  # Shims: any `pixel` the model types resolves to the pinned binary in the
  # pixel arm and to "command not found" in the no-pixel arm, never to the
  # managed install.
  mkdir -p "$KEY_DIR/pixel/shim" "$KEY_DIR/no-pixel/shim"
  printf '#!/bin/sh\nexec %s "$@"\n' "'$PIXEL_COPY'" >"$KEY_DIR/pixel/shim/pixel"
  printf '#!/bin/sh\necho "pixel: command not found" >&2\nexit 127\n' >"$KEY_DIR/no-pixel/shim/pixel"
  chmod +x "$KEY_DIR/pixel/shim/pixel" "$KEY_DIR/no-pixel/shim/pixel"
  cp "$KEY_DIR/no-pixel/shim/pixel" "$KEY_DIR/no-pixel/shim/pixel-dev"
  cp "$KEY_DIR/pixel/shim/pixel" "$KEY_DIR/pixel/shim/pixel-dev"
}

hook_payload() {
  jq -nc --arg cwd "$1" --arg p "$2" \
    '{hook_event_name:"UserPromptSubmit",session_id:"arena",cwd:$cwd,prompt:$p}'
}

# ---------------------------------------------------------------- codex home

# $1 home dir, $2 workspace, $3 hook binary or "" for none.
make_codex_home() {
  local home=$1 ws=$2 hookbin=$3 f=$USER_CODEX_HOME/config.toml
  [ -f "$USER_CODEX_HOME/auth.json" ] || die "no Codex login at $USER_CODEX_HOME/auth.json (run: codex login)"
  mkdir -p "$home"
  chmod 700 "$home"
  # The login is copied, never printed.
  cp "$USER_CODEX_HOME/auth.json" "$home/auth.json"
  chmod 600 "$home/auth.json"
  {
    # Top-level scalars the user's runs depend on; no hooks, MCP servers,
    # notify, plugins or marketplaces.
    awk '
      /^\[/ { exit }
      /^(approval_policy|model|model_reasoning_effort|personality|sandbox_mode|service_tier|project_doc_max_bytes|suppress_unstable_features_warning)[ ]*=/ { print }
    ' "$f" 2>/dev/null
    printf 'check_for_update_on_startup = false\n\n'
    printf '[features]\nhooks = true\nplugin_hooks = false\nskills = false\n\n'
    printf '[projects."%s"]\ntrust_level = "trusted"\n' "$ws"
  } >"$home/config.toml"
  if [ -n "$hookbin" ]; then
    jq -n --arg cmd "'$hookbin' run-hook task-event --provider codex --event prompt-submit" \
      '{hooks:{UserPromptSubmit:[{hooks:[{type:"command",command:$cmd,timeout:10}]}]}}' >"$home/hooks.json"
  fi
}

# ---------------------------------------------------------------- herdr

focused_workspace() {
  if [ -n "${HERDR_WORKSPACE_ID:-}" ]; then printf '%s' "$HERDR_WORKSPACE_ID"; return; fi
  hd workspace list | jq -r '[.result.workspaces[] | select(.focused)][0].workspace_id // .result.workspaces[0].workspace_id'
}

# Arena panes: TSV pane_id, tab_id, title. Herdr can reset a pane's label when
# the shell retitles it, so panes are matched by label or by the pane ids that
# runs/*/run.json recorded; the title shown is the intended one.
arena_panes() {
  local ids
  ids=$(find "$SCRATCH/runs" -name run.json -exec jq -s \
    '[.[] | {(.pixel_pane): ("arena:" + .name + ":pixel"), (.no_pixel_pane): ("arena:" + .name + ":no-pixel")}] | add // {}' {} + 2>/dev/null) || ids='{}'
  [ -n "$ids" ] || ids='{}'
  hd pane list | jq -r --argjson m "$ids" --arg p "$PREFIX:" \
    '.result.panes[] | (if (.label | startswith($p)) then .label else ($m[.pane_id] // empty) end) as $l | [.pane_id, .tab_id, $l] | @tsv'
}

titles() { # $1 pixel pane, $2 no-pixel pane, $3 case
  hd pane rename "$1" "$PREFIX:$3:pixel" >/dev/null 2>&1 || true
  hd pane rename "$2" "$PREFIX:$3:no-pixel" >/dev/null 2>&1 || true
}

agent_state() { hd agent get "$1" 2>/dev/null | jq -r '.result.agent.agent_status // "?"' 2>/dev/null || echo '?'; }

# Wait until the pane hosts a Codex agent that reports a ready state.
wait_ready() {
  local pane=$1 i=0 st
  while [ "$i" -lt 90 ]; do
    st=$(agent_state "$pane")
    case "$st" in idle|done) return 0 ;; esac
    sleep 1; i=$((i + 1))
  done
  return 1
}

# Codex asks a human to review hooks it did not write. The arena wrote this
# one, so trust it in this run's own CODEX_HOME.
review_hooks_if_asked() {
  local pane=$1 screen
  screen=$(hd agent read "$pane" --source visible 2>/dev/null || true)
  case "$screen" in
    *"Hooks need review"*)
      say "$pane: trusting the arena-written hook"
      hd agent send-keys "$pane" 2 >/dev/null
      hd agent send-keys "$pane" enter >/dev/null
      sleep 2 ;;
  esac
}

# ---------------------------------------------------------------- run

cmd_run() {
  local sel=${1:-}; [ -n "$sel" ] || die 'run needs a case number or name'; shift
  local bin='' sha='' timeout=600 focus=--no-focus
  while [ "$#" -gt 0 ]; do
    case "$1" in
      --pixel-bin) bin=${2:?--pixel-bin needs a path}; shift 2 ;;
      --sha) sha=${2:?--sha needs a commit}; shift 2 ;;
      --timeout) timeout=${2:?--timeout needs seconds}; shift 2 ;;
      --focus) focus=--focus; shift ;;
      *) die "unknown option: $1" ;;
    esac
  done
  [ -n "$bin" ] || bin=$(command -v pixel) || die 'no pixel binary: pass --pixel-bin'
  [ -x "$bin" ] || die "not executable: $bin"
  [ -x "$CODEX_BIN" ] || die "no Codex binary at $CODEX_BIN"

  local row n name prompt
  row=$(catalog | awk -F'\t' -v s="$sel" '$1 == s || $2 == s { print; exit }')
  [ -n "$row" ] || die "unknown case: $sel (see: arena.sh list)"
  IFS=$'\t' read -r n name prompt <<<"$row"

  if arena_panes | awk -F'\t' -v c="$PREFIX:$name:" 'index($3, c) == 1 { f = 1 } END { exit !f }'; then
    die "case $name already has arena panes; run: arena.sh abort $name"
  fi

  prepare "$bin" "$sha"
  local runid run_dir ph nh
  runid="$name-$(date -u +%Y%m%dT%H%M%SZ)"
  run_dir="$SCRATCH/runs/$runid"
  ph="$run_dir/pixel/codex-home"
  nh="$run_dir/no-pixel/codex-home"
  mkdir -p "$run_dir/pixel" "$run_dir/no-pixel"
  make_codex_home "$ph" "$PIXEL_WS" "$PIXEL_COPY"
  make_codex_home "$nh" "$NOPIXEL_WS" ""

  # Hook preflight, no model: the pixel hook yields the brief, the no-pixel
  # arm has no hook file and PIXEL_BRIEF=0 yields nothing.
  local pre_on pre_off
  pre_on=$(hook_payload "$PIXEL_WS" "$prompt" | "$PIXEL_COPY" run-hook task-event --provider codex --event prompt-submit 2>/dev/null || true)
  pre_off=$(hook_payload "$PIXEL_WS" "$prompt" | PIXEL_BRIEF=0 "$PIXEL_COPY" run-hook task-event --provider codex --event prompt-submit 2>/dev/null || true)
  case "$pre_on" in *'[PIXEL:BRIEF]'*) ;; *) die 'preflight: the pixel hook produced no [PIXEL:BRIEF]' ;; esac
  case "$pre_off" in *'[PIXEL:BRIEF]'*) die 'preflight: PIXEL_BRIEF=0 still produced a brief' ;; esac
  [ ! -e "$nh/hooks.json" ] || die 'preflight: no-pixel home has a hooks.json'

  local W tab_json tab_id p_pane n_pane
  W=$(focused_workspace)
  tab_json=$(hd tab create --workspace "$W" --cwd "$PIXEL_WS" --label "$PREFIX:$name" "$focus")
  tab_id=$(jq -r '.result.tab.tab_id' <<<"$tab_json")
  p_pane=$(jq -r '.result.root_pane.pane_id' <<<"$tab_json")
  n_pane=$(hd pane split "$p_pane" --direction right --ratio 0.5 --cwd "$NOPIXEL_WS" --no-focus | jq -r '.result.pane.pane_id')
  [ -n "$p_pane" ] && [ -n "$n_pane" ] || die 'could not create the two panes'
  titles "$p_pane" "$n_pane" "$name"
  jq -n --arg id "$runid" --arg tab "$tab_id" --arg pp "$p_pane" --arg np "$n_pane" --arg ph "$ph" --arg nh "$nh" \
    --arg n "$n" --arg name "$name" --arg prompt "$prompt" --arg sha "$SHA_FULL" --arg bh "$BIN_HASH" \
    '{run:$id,tab:$tab,pixel_pane:$pp,no_pixel_pane:$np,pixel_home:$ph,no_pixel_home:$nh,case:($n|tonumber),name:$name,prompt:$prompt,commit:$sha,pixel_binary_sha256:$bh}' >"$run_dir/run.json"
  say "panes: $PREFIX:$name:pixel=$p_pane  $PREFIX:$name:no-pixel=$n_pane  (tab $tab_id)"

  sleep 2
  local safe_path="/usr/bin:/bin:/usr/sbin:/sbin:/opt/homebrew/bin:$HOME/.cargo/bin:$HOME/.local/bin"
  hd pane run "$p_pane" "cd '$PIXEL_WS' && env CODEX_HOME='$ph' PATH='$KEY_DIR/pixel/shim':$safe_path '$CODEX_BIN'" >/dev/null
  hd pane run "$n_pane" "cd '$NOPIXEL_WS' && env CODEX_HOME='$nh' PIXEL_BRIEF=0 PATH='$KEY_DIR/no-pixel/shim':$safe_path '$CODEX_BIN'" >/dev/null
  wait_ready "$p_pane" || die "pixel arm never became ready (pane $p_pane)"
  wait_ready "$n_pane" || die "no-pixel arm never became ready (pane $n_pane)"
  review_hooks_if_asked "$p_pane"
  wait_ready "$p_pane" || die "pixel arm not ready after hook review"
  hd agent rename "$p_pane" "$PREFIX-$name-pixel" >/dev/null 2>&1 || true
  hd agent rename "$n_pane" "$PREFIX-$name-no-pixel" >/dev/null 2>&1 || true
  titles "$p_pane" "$n_pane" "$name"

  # Submit to both at the same time and wait for each to settle.
  local t0 t1 rc_p=0 rc_n=0
  t0=$(date +%s)
  hd agent prompt "$p_pane" "$prompt" --wait --timeout $((timeout * 1000)) >"$run_dir/pixel/prompt.json" 2>&1 & local pid_p=$!
  hd agent prompt "$n_pane" "$prompt" --wait --timeout $((timeout * 1000)) >"$run_dir/no-pixel/prompt.json" 2>&1 & local pid_n=$!
  wait "$pid_p" || rc_p=$?
  wait "$pid_n" || rc_n=$?
  t1=$(date +%s)

  titles "$p_pane" "$n_pane" "$name"
  collect_receipt "$run_dir" "$rc_p" "$rc_n" "$((t1 - t0))"
  summarize "$run_dir/receipt.json"
}

# ---------------------------------------------------------------- receipt

newest_rollout() { find "$1/sessions" -name 'rollout-*.jsonl' -type f 2>/dev/null | sort | tail -n 1; }

# One arm's facts from its Codex rollout, as JSON.
arm_json() {
  local home=$1 rc=$2 rollout
  rollout=$(newest_rollout "$home")
  if [ -z "$rollout" ]; then
    jq -n --arg rc "$rc" '{prompt_exit:($rc|tonumber),rollout:null,brief:null,tool_calls:[],final_answer:null,tokens:null,codex_seconds:null}'
    return
  fi
  jq -s --arg rollout "$rollout" --arg rc "$rc" '
    def txt: ((.content // []) | map(.text // .output_text // "") | join("\n"));
    [.[] | select(.type == "response_item") | .payload] as $items
    | [.[] | select(.type == "event_msg") | .payload] as $ev
    | {
        prompt_exit: ($rc | tonumber),
        rollout: $rollout,
        brief: ([$items[] | select(.type == "message") | txt | select(contains("[PIXEL:BRIEF]"))] | first // null),
        tool_calls: [ $items[]
          | select(.type == "function_call" or .type == "local_shell_call" or .type == "custom_tool_call")
          | { kind: .type, name: (.name // "shell"),
              call: (if .type == "local_shell_call" then ((.action.command // []) | join(" "))
                     else (.arguments // .input // "" | tostring
                       | (capture("cmd: ?\"(?<c>(\\\\.|[^\"])*)\"") | .c | gsub("\\\\n"; " ; ") | gsub("\\\\\""; "\"")) // .) end) } ],
        final_answer: ([$items[] | select(.type == "message" and .role == "assistant") | txt] | last // null),
        tokens: ([$ev[] | select(.type == "token_count") | .info.total_token_usage // empty] | last // null),
        codex_seconds: (
          ([.[] | select(.type == "event_msg" and .payload.type == "task_started") | .timestamp] | first) as $a
          | ([.[] | select(.type == "event_msg" and .payload.type == "task_complete") | .timestamp] | last) as $b
          | if $a and $b then (($b | sub("\\.[0-9]+Z$"; "Z") | fromdate) - ($a | sub("\\.[0-9]+Z$"; "Z") | fromdate)) else null end)
      }' "$rollout"
}

collect_receipt() {
  local run_dir=$1 rc_p=$2 rc_n=$3 wall=$4 pj nj ph nh
  ph=$(jq -r .pixel_home "$run_dir/run.json"); nh=$(jq -r .no_pixel_home "$run_dir/run.json")
  pj=$(arm_json "$ph" "$rc_p"); nj=$(arm_json "$nh" "$rc_n")
  jq --arg wall "$wall" --argjson pixel "$pj" --argjson nopixel "$nj" '
    { run: .run, case: .case, name: .name, prompt: .prompt, commit: .commit,
      pixel_binary_sha256: .pixel_binary_sha256, wall_seconds: ($wall | tonumber),
      isolation: {
        pixel_arm_received_brief: ($pixel.brief != null),
        no_pixel_arm_received_brief: ($nopixel.brief != null),
        ok: ($pixel.brief != null and $nopixel.brief == null) },
      arms: { "pixel": $pixel, "no-pixel": $nopixel } }' "$run_dir/run.json" >"$run_dir/receipt.json"
}

# Rebuild a receipt from the rollouts, e.g. after follow-up prompts.
cmd_receipt() {
  local run_dir=$SCRATCH/runs/${1:?receipt needs a run id}
  [ -f "$run_dir/run.json" ] || die "unknown run: $1"
  collect_receipt "$run_dir" 0 0 "$(jq -r '.wall_seconds // 0' "$run_dir/receipt.json" 2>/dev/null || echo 0)"
  summarize "$run_dir/receipt.json"
}

summarize() {
  local r=$1
  jq -r '
    def calls: .tool_calls | length;
    def tok: (.tokens.total_tokens // "n/a");
    "run \(.run)  commit \(.commit[0:12])  pixel \(.pixel_binary_sha256[0:12])  wall \(.wall_seconds)s",
    "isolation: pixel arm brief=\(.isolation.pixel_arm_received_brief) no-pixel arm brief=\(.isolation.no_pixel_arm_received_brief) -> \(if .isolation.ok then "OK" else "FAILED" end)",
    "                     pixel | no-pixel",
    "tool calls        \(.arms.pixel | calls) | \(.arms["no-pixel"] | calls)",
    "tokens            \(.arms.pixel | tok) | \(.arms["no-pixel"] | tok)",
    "codex seconds     \(.arms.pixel.codex_seconds // "n/a") | \(.arms["no-pixel"].codex_seconds // "n/a")",
    "tool calls in order:",
    (.arms | to_entries[] | .key as $k | .value.tool_calls | to_entries[] | "  [\($k)] \(.key + 1). \(.value.call | .[0:150])"),
    "answers:",
    (.arms | to_entries[] | "  [\(.key)] \((.value.final_answer // "none") | gsub("\n"; " ") | .[0:300])")' "$r"
  say "receipt: $r"
}

# ---------------------------------------------------------------- status / abort

cmd_status() {
  local rows
  rows=$(arena_panes)
  [ -n "$rows" ] || { echo 'no arena panes'; return 0; }
  while IFS=$'\t' read -r pane tab label; do
    printf '%s  tab %s  %s  %s\n' "$pane" "$tab" "$label" "$(agent_state "$pane")"
  done <<<"$rows"
}

cmd_abort() {
  local case_name=${1:-} match="$PREFIX:" pane tab label tabs=''
  [ -z "$case_name" ] || match="$PREFIX:$case_name:"
  while IFS=$'\t' read -r pane tab label; do
    [ -n "$pane" ] || continue
    case "$label" in "$match"*) ;; *) continue ;; esac
    say "interrupting and closing $label ($pane)"
    hd agent send-keys "$pane" ctrl+c >/dev/null 2>&1 || true
    sleep 1
    hd agent send-keys "$pane" ctrl+c >/dev/null 2>&1 || true
    hd pane close "$pane" >/dev/null 2>&1 || true
    tabs="$tabs $tab"
  done < <(arena_panes)
  # The tab disappears with its last pane; close it only if it is left empty
  # and still carries the arena label.
  for tab in $tabs; do
    if hd tab get "$tab" >/dev/null 2>&1; then
      label=$(hd tab get "$tab" | jq -r '.result.tab.label // ""')
      case "$label" in "$PREFIX:"*) hd tab close "$tab" >/dev/null 2>&1 || true ;; esac
    fi
  done
  # Stop the app-server daemon each CODEX_HOME leaves behind and drop the
  # copied Codex logins of the aborted runs.
  local d
  for d in "$SCRATCH"/runs/"${case_name:-*}"-*/; do
    [ -d "$d" ] || continue
    pkill -f -- "${d%/}/" 2>/dev/null || true
    rm -f "$d"pixel/codex-home/auth.json "$d"no-pixel/codex-home/auth.json
  done
  say 'abort complete'
}

# ---------------------------------------------------------------- main

case "${1:-}" in
  list) cmd_list ;;
  run) shift; cmd_run "$@" ;;
  status) cmd_status ;;
  receipt) shift; cmd_receipt "$@" ;;
  abort) shift; cmd_abort "$@" ;;
  -h|--help|help|'') usage ;;
  *) usage >&2; exit 2 ;;
esac
