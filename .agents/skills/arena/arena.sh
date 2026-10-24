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
PREFIX=${ARENA_PREFIX:-arena}
AGENT=codex
CLAUDE_BIN=${ARENA_CLAUDE_BIN:-$HOME/.local/bin/claude}
CLAUDE_PROJECTS=${ARENA_CLAUDE_PROJECTS:-$HOME/.claude/projects}
DEVIN_BIN=${ARENA_DEVIN_BIN:-$HOME/.local/bin/devin}
DEVIN_TRANSCRIPTS=${ARENA_DEVIN_TRANSCRIPTS:-$HOME/.local/share/devin/cli/transcripts}
CODEX_PROFILE=${ARENA_CODEX_PROFILE:-}
USER_DEVIN_CONFIG=${ARENA_USER_DEVIN_CONFIG:-$HOME/.config/devin/config.json}

usage() {
  cat <<'EOF'
Usage: arena.sh <command> [args]

  list                              the 8-case catalog: number, name, prompt
  run <n|name> [options]            run one case in two Codex panes
      --agent codex|claude          agent under test (default: codex)
      --pixel-bin PATH              Pixel binary for the pixel arm (default: pixel on PATH)
      --sha COMMIT                  commit both arms are cloned at (default: origin/main)
      --timeout SECONDS             idle wait per arm (default: 600)
      --here                        (default inside Herdr) split the two panes right in the current tab
      --tab                         open a new arena tab instead
                                    instead of opening a new arena tab
      --focus                       focus the arena tab when it opens
  run-set <source> [options]        run a whole set of questions in a row in the two panes
      source: catalog
      --keep-context (no /clear: one session per arm answers every question; writes session/ with all briefs and tool calls)   --fresh (a new pair per case)   --codex-profile NAME   --cross   --history   --agent codex|claude|devin   --pixel-bin PATH   --brief-extra CMD   --parts N   --only id,id   --timeout SECONDS   --sha COMMIT
      --here                        panes live in the current tab: the finished case's
                                    pair closes when the next case starts, the last
                                    pair stays open
  status                            arena panes and their agent state
  receipt <run>                     rebuild and print a run's receipt
  abort [<case>]                    interrupt the agents, close the arena panes,
                                    delete the copied login of those runs
EOF
}

die() { printf 'arena: %s\n' "$*" >&2; [ -z "${SET_LOCK_HELD:-}" ] || rmdir "$SET_LOCK_HELD" 2>/dev/null || true; exit 1; }
say() { printf 'arena: %s\n' "$*" >&2; }
hd() { HERDR_ENV=1 herdr "$@"; }

# ---------------------------------------------------------------- catalog

# TSV: number, name, prompt. SKILL.md holds the one copy of the catalog.
catalog() {
  awk '
    /^[0-9]+\. `[a-z0-9-]+` / {
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
    say 'indexing the pixel workspace (prepare --history)'
    (cd "$PIXEL_WS" && "$PIXEL_COPY" prepare . --history >"$KEY_DIR/pixel/prepare.log" 2>&1) \
      || die "indexing failed; see $KEY_DIR/pixel/prepare.log"
    touch "$KEY_DIR/pixel/indexed"
  fi
  # Warm the daemon (a cheap no-model hook call) and prove the no-pixel clone
  # carries no Pixel state at all.
  hook_payload "$PIXEL_WS" "warm" | "$PIXEL_COPY" hook prompt-submit --host codex >/dev/null 2>&1 || true
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
    printf '[orchestrator.mcp]\nenabled = false\n\n'
    printf '[projects."%s"]\ntrust_level = "trusted"\n' "$ws"
  } >"$home/config.toml"
  if [ -n "$CODEX_PROFILE" ]; then
    [ -f "$USER_CODEX_HOME/$CODEX_PROFILE.config.toml" ] || die "no Codex profile $USER_CODEX_HOME/$CODEX_PROFILE.config.toml"
    sed -E 's/^(hooks[[:space:]]*=[[:space:]]*)false/\1true/' "$USER_CODEX_HOME/$CODEX_PROFILE.config.toml" >"$home/$CODEX_PROFILE.config.toml"
    chmod 600 "$home/$CODEX_PROFILE.config.toml"
  fi
  if [ -n "$hookbin" ]; then
    jq -n --arg cmd "'$hookbin' hook prompt-submit --host codex" \
      '{hooks:{UserPromptSubmit:[{hooks:[{type:"command",command:$cmd,timeout:10}]}]}}' >"$home/hooks.json"
  fi
}

# Remove only the run-owned copies after the agent processes have exited and
# the receipt has captured their transcripts. Keep the homes and results.
cleanup_codex_auth_copies() {
  local run_dir=$1 home
  [ -f "$run_dir/run.json" ] || return 0
  for home in "$(jq -r '.pixel_home // empty' "$run_dir/run.json")" \
              "$(jq -r '.no_pixel_home // empty' "$run_dir/run.json")"; do
    case "$home" in
      "$run_dir/pixel/codex-home"|"$run_dir/no-pixel/codex-home")
        rm -f -- "$home/auth.json"
        ;;
    esac
  done
}



make_claude_home() {
  mkdir -p "$1"
  if [ -z "$2" ]; then echo '{"hooks":{}}' >"$1/settings.json"; return; fi
  jq -n --arg bin "$2" --argjson n "$3" '
    {hooks: {UserPromptSubmit: [range(1; $n + 1) as $k
      | {hooks: [{type: "command",
                  command: ("'"'"'" + $bin + "'"'"' hook prompt-submit --host claude"
                            + (if $n > 1 then " --part \($k)/\($n)" else "" end) + " || true"),
                  timeout: 10}]}]}}' >"$1/settings.json"
}

make_devin_home() {
  mkdir -p "$1"
  local hooks='{}'
  if [ -n "$2" ]; then
    hooks=$(jq -n --arg bin "$2" --argjson n "$3" '
      {UserPromptSubmit: [range(1; $n + 1) as $k
        | {hooks: [{type: "command",
                    command: ("'"'"'" + $bin + "'"'"' hook prompt-submit --host devin"
                              + (if $n > 1 then " --part \($k)/\($n)" else "" end) + " || true"),
                    timeout: 10}]}]}')
  fi
  jq --argjson h "$hooks" '.hooks = $h | .mcp = {} | .respect_workspace_trust = false' "$USER_DEVIN_CONFIG" >"$1/config.json"
  chmod 600 "$1/config.json"
}

claude_dialogs() {
  local pane=$1 screen
  screen=$(hd agent read "$pane" --source visible 2>/dev/null | tr '\n' ' ' | tr -s ' ' || true)
  case "$screen" in
    *"No, exit"*"trust this folder"*) hd agent send-keys "$pane" down >/dev/null; hd agent send-keys "$pane" enter >/dev/null; sleep 1 ;;
    *"Bypass Permissions"*|*"Yes, I accept"*) hd agent send-keys "$pane" 2 >/dev/null; hd agent send-keys "$pane" enter >/dev/null; sleep 1 ;;
  esac
}

agent_launch() {
  local arm=$1 ws=$2 home=$3 shim=$4 brief=''
  local safe_path="/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin:$HOME/.cargo/bin:$HOME/.local/bin"
  [ "$arm" = no-pixel ] && brief='PIXEL_BRIEF=0'
  [ "$arm" = pixel ] && brief="PIXEL_BRIEF_WINDOW_MS=${ARENA_BRIEF_WINDOW_MS:-8000}"
  if [ "$AGENT" = devin ]; then
    printf "cd '%s' && env %s PATH='%s':%s '%s' --config '%s/config.json' --permission-mode dangerous --respect-workspace-trust false" \
      "$ws" "$brief" "$shim" "$safe_path" "$DEVIN_BIN" "$home"
  elif [ "$AGENT" = claude ]; then
    printf "cd '%s' && env %s PATH='%s':%s '%s' --settings '%s/settings.json' --setting-sources project --strict-mcp-config --permission-mode bypassPermissions" \
      "$ws" "$brief" "$shim" "$safe_path" "$CLAUDE_BIN" "$home"
  elif [ "$arm" = pixel ]; then
    printf "cd '%s' && env CODEX_HOME='%s' %s PATH='%s':%s '%s' --dangerously-bypass-hook-trust%s" "$ws" "$home" "$brief" "$shim" "$safe_path" "$CODEX_BIN" "${CODEX_PROFILE:+ -p $CODEX_PROFILE}"
  else
    printf "cd '%s' && env CODEX_HOME='%s' %s PATH='%s':%s '%s'%s" "$ws" "$home" "$brief" "$shim" "$safe_path" "$CODEX_BIN" "${CODEX_PROFILE:+ -p $CODEX_PROFILE}"
  fi
}

check_agent() { case "$AGENT" in codex|claude|devin) ;; *) die "unknown agent: $AGENT (codex, claude or devin)" ;; esac; }

agent_bin_ok() {
  if [ "$AGENT" = claude ]; then [ -x "$CLAUDE_BIN" ] || die "no Claude binary at $CLAUDE_BIN (ARENA_CLAUDE_BIN)"
  elif [ "$AGENT" = devin ]; then [ -x "$DEVIN_BIN" ] || die "no Devin binary at $DEVIN_BIN (ARENA_DEVIN_BIN)"
  else [ -x "$CODEX_BIN" ] || die "no Codex binary at $CODEX_BIN"; fi
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
  hd pane list | jq -r --argjson m "$ids" --arg p "$PREFIX:" --arg s "$SCRATCH/runs/" \
    '.result.panes[] | (if ((.label // "") | startswith($p)) then .label
       elif ((.cwd // "") | startswith($s)) and ((.cwd // "") | test("/(pixel|no-pixel)/ws$")) then ($p + "orphan")
       else ($m[.pane_id] // empty) end) as $l | [.pane_id, .tab_id, $l] | @tsv'
}

titles() { # $1 pixel pane, $2 no-pixel pane, $3 case
  hd pane rename "$1" "$PREFIX:$3:with-pixel" >/dev/null 2>&1 || true
  hd pane rename "$2" "$PREFIX:$3:without-pixel" >/dev/null 2>&1 || true
}

# Drag column boundaries until every pane in the tab is ~1/N of the row width
# (herdr only nests binary splits). A zero-delta resize returns the layout.
equalize_columns() { # $1 any pane in the tab
  local anchor=$1 want=$2 k actual target delta dir moved lay w
  local -a P X
  for _ in $(seq $((want * 3))); do
    lay=$(hd pane resize --pane "$anchor" --direction left --amount 0 | jq -c '.result.resize.layout' 2>/dev/null) || { sleep 1; continue; }
    [ "$(jq '.panes | length' <<<"$lay")" = "$want" ] || { sleep 1; continue; }
    w=$(jq '.area.width' <<<"$lay")
    P=(); X=()  # bash 3.2: no mapfile
    while IFS=$'\t' read -r pp xx; do P+=("$pp"); X+=("$xx"); done \
      < <(jq -r '.panes | sort_by(.rect.x) | .[] | [.pane_id, .rect.x] | @tsv' <<<"$lay")
    anchor=${P[1]}  # re-probe a mid pane; keeps working if $1 was retitled
    moved=0
    for ((k = 1; k < want; k++)); do
      target=$((w * k / want)); actual=${X[$k]}; delta=$((actual - target))
      [ ${delta#-} -le 1 ] && continue
      if [ "$delta" -gt 0 ]; then dir=left; else dir=right; delta=$((-delta)); fi
      hd pane resize --pane "${P[$k]}" --direction "$dir" --amount "$(awk -v d="$delta" -v w="$w" 'BEGIN{printf "%.3f", d/w}')" >/dev/null 2>&1 || true
      moved=1; break  # one drag per probe: geometry shifts under nested splits
    done
    [ "$moved" = 1 ] || return 0
  done
}

# Open the two panes of a case: a new arena tab, or with $4=1 two splits in the
# current tab beside the caller, columns equalized. Sets TAB_ID ("" in --here),
# P_PANE, N_PANE.
make_panes() { # $1 label, $2 pixel cwd, $3 no-pixel cwd, $4 here, $5 focus flag
  if [ "$4" = "1" ]; then
    TAB_ID=''
    P_PANE=$(hd pane split --current --direction right --cwd "$2" --no-focus | jq -r '.result.pane.pane_id')
    N_PANE=$(hd pane split "$P_PANE" --direction right --cwd "$3" --no-focus | jq -r '.result.pane.pane_id')
    equalize_columns "$P_PANE" 3
    return
  fi
  local W tj
  W=$(focused_workspace)
  tj=$(hd tab create --workspace "$W" --cwd "$2" --label "$PREFIX:$1" "$5")
  TAB_ID=$(jq -r '.result.tab.tab_id' <<<"$tj")
  P_PANE=$(jq -r '.result.root_pane.pane_id' <<<"$tj")
  N_PANE=$(hd pane split "$P_PANE" --direction right --ratio 0.5 --cwd "$3" --no-focus | jq -r '.result.pane.pane_id')
}

agent_state() { hd agent get "$1" 2>/dev/null | jq -r '.result.agent.agent_status // "?"' 2>/dev/null || echo '?'; }

# Wait until the pane hosts a Codex agent that reports a ready state.
wait_ready() {
  local pane=$1 i=0 st
  while [ "$i" -lt 90 ]; do
    st=$(agent_state "$pane")
    [ "$AGENT" != claude ] || claude_dialogs "$pane"
    case "$st" in idle|done) return 0 ;; esac
    sleep 1; i=$((i + 1))
  done
  return 1
}

# Codex asks a human to review hooks it did not write, either as the modal
# "Hooks need review" (key 2 trusts) or as the /hooks panel "N hooks need
# review" (t trusts all, esc closes it). The arena wrote this hook, so it is
# trusted in this run's own CODEX_HOME; the panel appears when the first
# prompt-submit actually runs the hooks, not necessarily at session start.
review_hooks_if_asked() {
  local pane=$1 screen
  [ "$AGENT" = codex ] || return 0
  screen=$(hd agent read "$pane" --source visible 2>/dev/null || true)
  case "$screen" in
    *"Hooks need review"*)
      say "$pane: trusting the arena-written hook"
      hd agent send-keys "$pane" 2 >/dev/null
      hd agent send-keys "$pane" enter >/dev/null
      sleep 2 ;;
    *"need review"*)
      say "$pane: trusting the arena-written hook"
      hd agent send-keys "$pane" t >/dev/null
      sleep 1
      hd agent send-keys "$pane" esc >/dev/null
      sleep 2 ;;
  esac
}

# ---------------------------------------------------------------- run

cmd_run() {
  local sel=${1:-}; [ -n "$sel" ] || die 'run needs a case number or name'; shift
  local bin='' sha='' timeout=600 focus=--no-focus here=${ARENA_HERE:-$([ -n "${HERDR_PANE_ID:-}" ] && echo 1 || echo 0)}
  while [ "$#" -gt 0 ]; do
    case "$1" in
      --agent) AGENT=${2:?--agent needs codex or claude}; shift 2 ;;
      --codex-profile) CODEX_PROFILE=${2:?--codex-profile needs a name}; shift 2 ;;
      --pixel-bin) bin=${2:?--pixel-bin needs a path}; shift 2 ;;
      --sha) sha=${2:?--sha needs a commit}; shift 2 ;;
      --timeout) timeout=${2:?--timeout needs seconds}; shift 2 ;;
      --here) here=1; shift ;;
      --tab) here=0; shift ;;
      --focus) focus=--focus; shift ;;
      *) die "unknown option: $1" ;;
    esac
  done
  check_agent
  [ -n "$bin" ] || bin=$(command -v pixel) || die 'no pixel binary: pass --pixel-bin'
  [ -x "$bin" ] || die "not executable: $bin"
  agent_bin_ok

  local row n name prompt
  row=$(catalog | awk -F'\t' -v s="$sel" '$1 == s || $2 == s { print; exit }')
  [ -n "$row" ] || die "unknown case: $sel (see: arena.sh list)"
  IFS=$'\t' read -r n name prompt <<<"$row"

  if [ "$here" = 1 ]; then
    close_all_arena_panes
  elif arena_panes | awk -F'\t' -v c="$PREFIX:$name:" 'index($3, c) == 1 { f = 1 } END { exit !f }'; then
    say "closing the stale arena panes of case $name"
    close_case_tab "$name"
  fi

  prepare "$bin" "$sha"
  local runid run_dir ph nh
  runid="$name-$(date -u +%Y%m%dT%H%M%SZ)"
  run_dir="$SCRATCH/runs/$runid"
  ph="$run_dir/pixel/codex-home"
  nh="$run_dir/no-pixel/codex-home"
  mkdir -p "$run_dir/pixel" "$run_dir/no-pixel"
  if [ "$AGENT" = claude ]; then
    make_claude_home "$ph" "$PIXEL_COPY" 1
    make_claude_home "$nh" "" 1
  elif [ "$AGENT" = devin ]; then
    make_devin_home "$ph" "$PIXEL_COPY" 1
    make_devin_home "$nh" "" 1
  else
    make_codex_home "$ph" "$PIXEL_WS" "$PIXEL_COPY"
    make_codex_home "$nh" "$NOPIXEL_WS" ""
  fi

  # Hook preflight, no model: the pixel hook yields the brief, the no-pixel
  # arm has no hook file and PIXEL_BRIEF=0 yields nothing.
  local pre_on pre_off
  pre_on=$(hook_payload "$PIXEL_WS" "$prompt" | "$PIXEL_COPY" hook prompt-submit --host "$AGENT" 2>/dev/null || true)
  pre_off=$(hook_payload "$PIXEL_WS" "$prompt" | PIXEL_BRIEF=0 "$PIXEL_COPY" hook prompt-submit --host "$AGENT" 2>/dev/null || true)
  case "$pre_on" in *'[PIXEL:BRIEF]'*) ;; *) die 'preflight: the pixel hook produced no [PIXEL:BRIEF]' ;; esac
  case "$pre_off" in *'[PIXEL:BRIEF]'*) die 'preflight: PIXEL_BRIEF=0 still produced a brief' ;; esac
  [ ! -e "$nh/hooks.json" ] || die 'preflight: no-pixel home has a hooks.json'
  [ "$AGENT" != claude ] || ! grep -q 'hook prompt-submit' "$nh/settings.json" || die 'preflight: no-pixel settings carry a hook'

  local tab_id p_pane n_pane
  make_panes "$name" "$PIXEL_WS" "$NOPIXEL_WS" "$here" "$focus"
  p_pane=$P_PANE; n_pane=$N_PANE; tab_id=$TAB_ID
  [ -n "$p_pane" ] && [ -n "$n_pane" ] || die 'could not create the two panes'
  titles "$p_pane" "$n_pane" "$name"
  jq -n --arg id "$runid" --arg tab "$tab_id" --arg pp "$p_pane" --arg np "$n_pane" --arg ph "$ph" --arg nh "$nh" \
    --arg n "$n" --arg name "$name" --arg prompt "$prompt" --arg sha "$SHA_FULL" --arg bh "$BIN_HASH" \
    '{run:$id,tab:$tab,pixel_pane:$pp,no_pixel_pane:$np,pixel_home:$ph,no_pixel_home:$nh,case:($n|tonumber),name:$name,prompt:$prompt,commit:$sha,pixel_binary_sha256:$bh}' >"$run_dir/run.json"
  run_json_extend "$run_dir" "$PIXEL_WS" "$NOPIXEL_WS"
  say "panes: $PREFIX:$name:pixel=$p_pane  $PREFIX:$name:no-pixel=$n_pane  (tab $tab_id)"

  sleep 2
  hd pane run "$p_pane" "$(agent_launch pixel "$PIXEL_WS" "$ph" "$KEY_DIR/pixel/shim")" >/dev/null
  hd pane run "$n_pane" "$(agent_launch no-pixel "$NOPIXEL_WS" "$nh" "$KEY_DIR/no-pixel/shim")" >/dev/null
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
  # The review panel opens when the prompt actually runs the hooks: patrol.
  ( for _ in $(seq 60); do review_hooks_if_asked "$p_pane"; sleep 3; done ) & local pid_t=$!
  wait "$pid_p" || rc_p=$?
  wait "$pid_n" || rc_n=$?
  kill "$pid_t" 2>/dev/null || true; wait "$pid_t" 2>/dev/null || true
  t1=$(date +%s)

  titles "$p_pane" "$n_pane" "$name"
  collect_receipt "$run_dir" "$rc_p" "$rc_n" "$((t1 - t0))"
  cleanup_codex_auth_copies "$run_dir"
  summarize "$run_dir/receipt.json"
}

# ---------------------------------------------------------------- receipt

newest_rollout() { find "$1/sessions" -name 'rollout-*.jsonl' -type f 2>/dev/null | sort | tail -n 1; }

run_json_extend() {
  jq --arg a "$AGENT" --arg pw "$2" --arg nw "$3" '. + {agent:$a, pixel_ws:$pw, no_pixel_ws:$nw}' "$1/run.json" >"$1/run.json.tmp" \
    && mv -f "$1/run.json.tmp" "$1/run.json"
}

newest_transcript() {
  local ws dir
  ws=$(cd "$1" 2>/dev/null && pwd -P) || return 0
  dir="$CLAUDE_PROJECTS/$(printf '%s' "$ws" | sed 's/[^A-Za-z0-9-]/-/g')"
  ls -t "$dir"/*.jsonl 2>/dev/null | head -n 1
}

claude_arm_json() {
  local file; file=$(newest_transcript "$1")
  if [ -z "$file" ]; then
    jq -n --arg rc "$2" '{prompt_exit:($rc|tonumber),rollout:null,brief:null,tool_calls:[],final_answer:null,tokens:null,codex_seconds:null}'
    return
  fi
  jq -s --arg rollout "$file" --arg rc "$2" '
    def ts: sub("\\.[0-9]+Z$"; "Z") | fromdate;
    def blocks: if (.message.content | type) == "array" then .message.content else [] end;
    [.[] | select(.type == "assistant")] as $as
    | ([.[] | select(.type == "attachment" and .attachment.type == "hook_additional_context" and .attachment.hookEvent == "UserPromptSubmit")
        | .attachment.content | if type == "array" then join("\n") else tostring end | select(length > 0)]) as $ctx
    | { prompt_exit: ($rc | tonumber), rollout: $rollout,
        brief: (if ($ctx | map(contains("[PIXEL:BRIEF]")) | any) then ($ctx | join("\n")) else null end),
        tool_calls: [ $as[] | blocks[] | select(.type == "tool_use")
          | { kind: "tool_use", name: .name,
              call: (.input.command // .input.file_path // .input.pattern // .input.query // (.input | tostring)) } ],
        final_answer: ([$as[] | blocks[] | select(.type == "text") | .text] | last // null),
        tokens: ([$as | group_by(.message.id)[] | last.message.usage // empty]
                 | if length == 0 then null else {total_tokens: (map((.input_tokens // 0) + (.output_tokens // 0) + (.cache_creation_input_tokens // 0) + (.cache_read_input_tokens // 0)) | add)} end),
        codex_seconds: (if ($as | length) > 0 then (($as | last | .timestamp | ts) - ([.[] | select(.type == "user" and .timestamp) | .timestamp] | first | ts)) else null end) }' "$file"
}

newest_devin_transcript() {
  find "$DEVIN_TRANSCRIPTS" -name '*.json' -mmin -240 -type f 2>/dev/null | xargs grep -l -F -- "$1" 2>/dev/null | xargs ls -t 2>/dev/null | head -n 1
}

devin_arm_json() {
  local file; file=$(newest_devin_transcript "$(cd "$1" 2>/dev/null && pwd -P)")
  if [ -z "$file" ]; then
    jq -n --arg rc "$2" '{prompt_exit:($rc|tonumber),rollout:null,brief:null,tool_calls:[],final_answer:null,tokens:null,codex_seconds:null}'
    return
  fi
  jq --arg rollout "$file" --arg rc "$2" '
    def ts: sub("\\.[0-9]+(\\+00:00|Z)$"; "Z") | fromdate;
    [.steps[] | select(.source == "agent")] as $as
    | ([.steps[] | select(.source == "user") | .timestamp] | first) as $a
    | (.steps | last | .timestamp) as $b
    | ([.steps[] | select(.source != "agent" and ((.message | tostring) | contains("[PIXEL:BRIEF]")) and ((.extra.telemetry.source // "") != "rules")) | .message | tostring]) as $ctx
    | { prompt_exit: ($rc | tonumber), rollout: $rollout,
        brief: (if ($ctx | length) > 0 then ($ctx | join("\n")) else null end),
        tool_calls: [ $as[] | (.tool_calls // [])[] | { kind: "tool_call", name: .function_name,
              call: (.arguments.command // .arguments.file_path // .arguments.pattern // .arguments.query // (.arguments | tostring)) } ],
        final_answer: ([$as[] | select((.tool_calls // []) | length == 0) | (.message | tostring)] | last // null),
        tokens: (if .final_metrics then {total_tokens: ((.final_metrics.total_prompt_tokens // 0) + (.final_metrics.total_completion_tokens // 0))} else null end),
        codex_seconds: (if $a and $b then (($b | ts) - ($a | ts)) else null end) }' "$file"
}

arm_json() {
  local home=$1 rc=$2 rollout ws=${3:-}
  if [ "$AGENT" = claude ]; then claude_arm_json "$ws" "$rc"; return; fi
  if [ "$AGENT" = devin ]; then devin_arm_json "$ws" "$rc"; return; fi
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
  AGENT=$(jq -r '.agent // "codex"' "$run_dir/run.json")
  pj=$(arm_json "$ph" "$rc_p" "$(jq -r '.pixel_ws // empty' "$run_dir/run.json")")
  nj=$(arm_json "$nh" "$rc_n" "$(jq -r '.no_pixel_ws // empty' "$run_dir/run.json")")
  jq --arg wall "$wall" --argjson pixel "$pj" --argjson nopixel "$nj" '
    { run: .run, case: .case, name: .name, prompt: .prompt, commit: .commit,
      pixel_binary_sha256: .pixel_binary_sha256, wall_seconds: ($wall | tonumber),
      isolation: {
        pixel_arm_received_brief: ($pixel.brief != null),
        no_pixel_arm_received_brief: ($nopixel.brief != null),
        ok: ($pixel.brief != null and $nopixel.brief == null) },
      arms: { "pixel": $pixel, "no-pixel": $nopixel } }' "$run_dir/run.json" >"$run_dir/receipt.json"
  # The exact brief text the pixel arm's Codex received — a run's main output.
  jq -r '.arms.pixel.brief // empty' "$run_dir/receipt.json" >"$run_dir/brief.md"
}

# Rebuild a receipt from the rollouts, e.g. after follow-up prompts.
cmd_receipt() {
  local run_dir=$SCRATCH/runs/${1:?receipt needs a run id}
  [ -f "$run_dir/run.json" ] || die "unknown run: $1"
  collect_receipt "$run_dir" 0 0 "$(jq -r '.wall_seconds // 0' "$run_dir/receipt.json" 2>/dev/null || echo 0)"
  cleanup_codex_auth_copies "$run_dir"
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
    "agent seconds     \(.arms.pixel.codex_seconds // "n/a") | \(.arms["no-pixel"].codex_seconds // "n/a")",
    "tool calls in order:",
    (.arms | to_entries[] | .key as $k | .value.tool_calls | to_entries[] | "  [\($k)] \(.key + 1). \(.value.call | .[0:150])"),
    "answers:",
    (.arms | to_entries[] | "  [\(.key)] \((.value.final_answer // "none") | gsub("\n"; " ") | .[0:300])")' "$r"
  say "receipt: $r"
  # Print the brief the pixel arm's Codex actually received.
  local brief; brief=$(jq -r '.arms.pixel.brief // empty' "$r")
  if [ -n "$brief" ]; then
    say '----- pixel brief (injected into the with-pixel arm) -----'
    printf '%s\n' "$brief"
    say '----- end of pixel brief -----'
  fi
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
  if [ -n "$case_name" ]; then
    case "$case_name" in *[!a-z0-9-]*) die 'abort case must be a catalog name' ;; esac
    catalog | awk -F '\t' -v wanted="$case_name" '$2 == wanted { found = 1 } END { exit !found }' \
      || die "unknown abort case: $case_name"
  fi
  if [ -z "$case_name" ] && [ -f "$SET_PID_FILE" ] && kill -0 "$(cat "$SET_PID_FILE")" 2>/dev/null; then
    say "stopping the running set (pid $(cat "$SET_PID_FILE"))"
    kill -TERM "$(cat "$SET_PID_FILE")" 2>/dev/null || true
    sleep 2
  fi
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
  if [ -d "$SCRATCH/runs" ]; then
    while IFS= read -r -d '' d; do
      pkill -f -- "$d/" 2>/dev/null || true
      cleanup_codex_auth_copies "$d"
    done < <(find "$SCRATCH/runs" -mindepth 1 -maxdepth 1 -type d \
      -name "${case_name:-*}-*" -print0)
  fi
  local ip
  for ip in $(pgrep -f 'pixel prepare' 2>/dev/null); do
    case "$(lsof -a -p "$ip" -d cwd -Fn 2>/dev/null | sed -n 's/^n//p')" in "$SCRATCH"/setws/*) kill "$ip" 2>/dev/null || true ;; esac
  done
  rm -rf "$SCRATCH"/setws/*.lock
  say 'abort complete'
}


# ---------------------------------------------------------------- run-set

SET_PID_FILE=${ARENA_PID_FILE:-$SCRATCH/set.pid}

# TSV rows: id, repo, source directory, sha, prompt. Census prompts are read
# here and go to the panes and nowhere else (no log, no echo).
set_cases() {
  local source=$1 sha_arg=$2 root full
  case "$source" in
    catalog)
      root=$(git -C "$SELF_DIR" rev-parse --git-common-dir) || die 'run from a pixel checkout'
      root=$(cd "$SELF_DIR" && cd "$root" && pwd)
      [ -n "$sha_arg" ] || { git -C "$root" fetch -q origin main || true; sha_arg=origin/main; }
      full=$(git -C "$root" rev-parse --verify "$sha_arg^{commit}") || die "unknown commit: $sha_arg"
      local csrc="$SCRATCH/src/pixel"
      if [ ! -d "$csrc/.git" ]; then mkdir -p "$SCRATCH/src"; git clone -q --no-checkout "$root" "$csrc" >&2; fi
      git -C "$csrc" fetch -q "$root" "+refs/heads/*:refs/remotes/local/*" >&2 || true
      git -C "$csrc" cat-file -e "$full^{commit}" 2>/dev/null || git -C "$csrc" fetch -q "$root" "$full" >&2
      git -C "$csrc" checkout -q --detach "$full" >&2
      catalog | awk -F'\t' -v sha="$full" -v src="$csrc" '{ printf "%s\tpixel\t%s\t%s\t%s\n", $2, src, sha, $3 }' ;;
    *) die "unknown source: $source (catalog)" ;;
  esac
}

# Copy-on-write copy of a clone, at its pinned commit and without Pixel state.
cow_clone() { # $1 source dir, $2 dest, $3 sha
  rm -rf "$2"; mkdir -p "$(dirname "$2")"
  cp -c -R "$1" "$2"
  if [ "$(git -C "$2" rev-parse HEAD)" != "$3" ]; then git -C "$2" checkout -q --detach "$3" || die "cannot check out $3 in $2"; fi
  rm -rf "$2/.pixel"
}

# Index one repository at one commit with one binary, once; sets SET_BASE.
set_base() { # $1 repo, $2 source dir, $3 sha, $4 binary, $5 binary hash
  local key="$1-${3:0:12}-${5:0:12}"
  SET_BASE="$SCRATCH/setws/$key"
  local lock="$SCRATCH/setws/$key.lock"
  mkdir -p "$SCRATCH/setws"
  while [ ! -f "$SET_BASE/indexed" ] && ! mkdir "$lock" 2>/dev/null; do sleep 2; done
  if [ ! -f "$SET_BASE/indexed" ]; then
    SET_LOCK_HELD=$lock
    say "indexing $1 at ${3:0:12} (once per repository, commit and binary)"
    rm -rf "$SET_BASE"
    local seed; seed=$(ls -dt "$SCRATCH/setws/$1-${3:0:12}"-*/ 2>/dev/null | while read -r d; do [ -f "${d}indexed" ] && { echo "${d%/}"; break; }; done || true)
    if [ -n "$seed" ]; then
      say "seeding $1 from ${seed##*/}"
      mkdir -p "$SET_BASE"; cp -c -R "$seed/pixel" "$SET_BASE/pixel"; cp -c -R "$seed/no-pixel" "$SET_BASE/no-pixel"
    else
      cow_clone "$2" "$SET_BASE/pixel/ws" "$3"
      cow_clone "$2" "$SET_BASE/no-pixel/ws" "$3"
    fi
    ( cd "$SET_BASE/pixel/ws" && "$4" prepare . >"$SET_BASE/prepare.log" 2>&1 ) || die "pixel prepare failed; see $SET_BASE/prepare.log"
    # The history index is optional (a blobless clone can take longer than the watchdog allows) but a build that
    # fails by itself is not: only the watchdog's own kill is tolerated, and it is logged.
    if [ "${ARENA_HISTORY:-0}" = 1 ]; then
    ( cd "$SET_BASE/pixel/ws"
      "$4" prepare . --history >>"$SET_BASE/prepare.log" 2>&1 & hp=$!
      ( sleep 420; if kill -0 "$hp" 2>/dev/null; then echo 'pixel prepare history watchdog fired after 420 seconds' >>"$SET_BASE/prepare.log"; kill "$hp" 2>/dev/null || true; fi ) & wp=$!
      if wait "$hp"; then rc=0; else rc=$?; fi
      kill "$wp" 2>/dev/null || true; wait "$wp" 2>/dev/null || true
      if [ "$rc" -ne 0 ] && ! grep -q 'watchdog fired' "$SET_BASE/prepare.log"; then exit "$rc"; fi ) || die "pixel prepare --history failed; see $SET_BASE/prepare.log"
    else
      ( cd "$SET_BASE/pixel/ws" && "$4" prepare . >>"$SET_BASE/prepare.log" 2>&1 ) || die "pixel prepare failed; see $SET_BASE/prepare.log"
    fi
    ( cd "$SET_BASE/pixel/ws" && "$4" search-meaning 'how does this code work' . >>"$SET_BASE/prepare.log" 2>&1 ) || die "search-meaning failed; see $SET_BASE/prepare.log"
    touch "$SET_BASE/indexed"
  fi
  rmdir "$lock" 2>/dev/null || true
  SET_LOCK_HELD=''
}

# A daemon that has just started answers the first briefs empty: ask until the
# same text comes back three times.
settle_daemon() { # $1 binary, $2 workspace
  local last='' same=0 out i=0
  while [ "$i" -lt 10 ]; do
    out=$(cd "$2" && "$1" brief --metrics off 'where is the main entry point of this program defined' . 2>/dev/null || true)
    if [ "$out" = "$last" ]; then same=$((same + 1)); else same=1; fi
    last=$out
    [ "$same" -ge 3 ] && return 0
    sleep 1; i=$((i + 1))
  done
}

# hooks.json with $3 UserPromptSubmit hooks (parts), the way pixel setup writes them.
write_part_hooks() { # $1 home, $2 hook binary, $3 parts
  jq -n --arg bin "$2" --argjson n "$3" '
    {hooks: {UserPromptSubmit: [range(1; $n + 1) as $k
      | {hooks: [{type: "command",
                  command: ("'"'"'" + $bin + "'"'"' hook prompt-submit --host codex"
                            + (if $n > 1 then " --part \($k)/\($n)" else "" end) + " || true"),
                  timeout: 10}]}]}}' >"$1/hooks.json"
}

# Bytes the pixel arm's hooks printed, from the clone's own feedback log.
brief_bytes_of() {
  local w=$1/.pixel/hook-wrap.jsonl f=$1/.pixel/brief-feedback.jsonl
  # A hydrated brief: what the wrapper delivered (the hooks' own log holds the unprobed brief).
  if [ -f "$w" ]; then jq -s '[.[] | select(.t == "wrapped") | .bytes] | last // 0' "$w"; return; fi
  [ -f "$f" ] || { echo 0; return; }
  jq -s '[.[] | select(.t == "part") | .bytes] | add // 0' "$f" 2>/dev/null || echo 0
}

close_case_tab() { # $1 case name: close the arena panes and tab of a finished case
  local pane tab label
  while IFS=$'\t' read -r pane tab label; do
    case "$label" in "$PREFIX:$1:"*) hd pane close "$pane" >/dev/null 2>&1 || true ;; esac
  done < <(arena_panes)
}

close_all_arena_panes() { # every arena pane left by any earlier run — --here mode shares the caller tab
  local pane
  while IFS=$'\t' read -r pane _ _; do
    hd pane close "$pane" >/dev/null 2>&1 || true
  done < <(arena_panes | awk -F'\t' -v p="$PREFIX:" 'index($3, p) == 1')
}

set_abort() {
  say 'abort: interrupting the set'
  [ -z "${SET_PREWARM:-}" ] || { pkill -P "$SET_PREWARM" 2>/dev/null || true; kill "$SET_PREWARM" 2>/dev/null || true; }
  [ -z "${SET_PIDS:-}" ] || kill $SET_PIDS 2>/dev/null || true
  [ -z "${SET_CASE:-}" ] || cmd_abort "$SET_CASE" >/dev/null 2>&1 || true
  rm -f "$SET_PID_FILE"
  [ -z "${SET_OUT:-}" ] || set_table "$SET_OUT" || true
  exit 130
}

# The table: case | no-pixel calls | pixel calls | brief bytes | pixel 0? and totals.
set_table() { # $1 set dir
  local f=$1/results.jsonl
  [ -s "$f" ] || { say 'no case finished'; return 0; }
  jq -rs '
    def zero: if .pixel_calls == 0 and .brief_bytes > 0 then "yes"
              elif .pixel_calls == 0 then "0 but NO BRIEF delivered" else "no" end;
    ["| case | no-pixel calls | pixel calls | brief bytes | pixel 0? |", "|---|---:|---:|---:|---|"]
    + [ .[] | "| \(.id) | \(.no_pixel_calls) | \(.pixel_calls) | \(.brief_bytes) | \(zero) |" ]
    + ["",
       "totals: no-pixel \([.[].no_pixel_calls] | add) calls, pixel \([.[].pixel_calls] | add) calls over \(length) cases; "
       + "pixel at 0 in \([.[] | select(.pixel_calls == 0 and .brief_bytes > 0)] | length), "
       + "no brief delivered in \([.[] | select(.brief_bytes == 0)] | length)",
       "Arena observations only; no baseline comparison is included"]
    | .[]' "$f" | tee "$1/table.md"
  say "table: $1/table.md"
}

prewarm_bases() {
  local id repo srcdir csha prompt running=0 seen=',' first=1
  while IFS=$'\t' read -r id repo srcdir csha prompt; do
    [ -n "$id" ] || continue
    if [ -n "$2" ] && ! printf ',%s,' "$2" | grep -q ",$id,"; then continue; fi
    case "$seen" in *",$repo-$csha,"*) continue ;; esac
    seen="$seen$repo-$csha,"
    if [ "$first" = 1 ]; then first=0; continue; fi
    [ -d "$srcdir/.git" ] || continue
    ( set_base "$repo" "$srcdir" "$csha" "$3" "$4" ) >/dev/null 2>&1 &
    running=$((running + 1))
    if [ "$running" -ge 2 ]; then wait -n 2>/dev/null || wait; running=$((running - 1)); fi
  done <<<"$1"
  wait
}
set_unlock_stale() { rm -rf "$SCRATCH"/setws/*.lock; }

session_pair_write() { # $1 source, $2 pair number, $3 first case of the pair, $4 pixel workspace, $5 no-pixel workspace
  local sd="$SET_OUT/session" sfx='' pt nt
  pt=$(newest_transcript "$4"); nt=$(newest_transcript "$5")
  [ -n "$pt" ] && [ -n "$nt" ] || { say "session: no transcript for the pair that starts at $3, not written"; return 0; }
  [ "$2" = 1 ] || sfx=-$2
  mkdir -p "$sd"
  cp "$pt" "$sd/pixel.session$sfx.jsonl"; cp "$nt" "$sd/no-pixel.session$sfx.jsonl"
  python3 -I "$SELF_DIR/session-digest.py" "$pt" >"$sd/pixel.turns$sfx.json"
  python3 -I "$SELF_DIR/session-digest.py" "$nt" >"$sd/no-pixel.turns$sfx.json"
  if [ "$2" = 1 ]; then : >"$sd/report.md"; else printf '\n---\n\nPair %s, from case %s\n\n' "$2" "$3" >>"$sd/report.md"; fi
  python3 -I "$SELF_DIR/session-digest.py" "$pt" --md "$nt" >>"$sd/report.md"
}

cmd_run_set() {
  local source=${1:-}; [ -n "$source" ] || die 'run-set needs a source: catalog'; shift
  local bin='' sha='' timeout=600 extra='' parts=3 only='' cross=0 here=${ARENA_HERE:-$([ -n "${HERDR_PANE_ID:-}" ] && echo 1 || echo 0)}
  while [ "$#" -gt 0 ]; do
    case "$1" in
      --pixel-bin) bin=${2:?--pixel-bin needs a path}; shift 2 ;;
      --agent) AGENT=${2:?--agent needs codex or claude}; shift 2 ;;
      --history) ARENA_HISTORY=1; shift ;;
      --codex-profile) CODEX_PROFILE=${2:?--codex-profile needs a name}; shift 2 ;;
      --cross) cross=1; shift ;;
      --fresh) ARENA_REUSE=0; shift ;;
      --keep-context) ARENA_CLEAR=0; shift ;;
      --brief-extra) extra=${2:?--brief-extra needs a command}; shift 2 ;;
      --parts) parts=${2:?--parts needs a number}; shift 2 ;;
      --only) only=${2:?--only needs ids}; shift 2 ;;
      --timeout) timeout=${2:?--timeout needs seconds}; shift 2 ;;
      --sha) sha=${2:?--sha needs a commit}; shift 2 ;;
      --here) here=1; shift ;;
      --tab) here=0; shift ;;
      *) die "unknown option: $1" ;;
    esac
  done
  [ -n "$bin" ] || bin=$(command -v pixel) || die 'no pixel binary: pass --pixel-bin'
  [ -x "$bin" ] || die "not executable: $bin"
  check_agent; agent_bin_ok
  [ "${ARENA_CLEAR:-1}" = 1 ] || [ "$AGENT" = claude ] || die '--keep-context reads Claude Code transcripts: use --agent claude'
  [ "${ARENA_CLEAR:-1}" = 1 ] || [ "${ARENA_REUSE:-1}" = 1 ] || die '--keep-context needs the panes of a repository kept: drop --fresh'
  local bin_hash; bin_hash=$(sha256_of "$bin")
  SET_OUT="$SCRATCH/sets/$source-$(date -u +%Y%m%dT%H%M%SZ)"
  mkdir -p "$SET_OUT"
  : >"$SET_OUT/results.jsonl"
  echo $$ >"$SET_PID_FILE"
  trap set_abort INT TERM
  SET_CASE=''; SET_PIDS=''; SET_PREWARM=''
  local cases; cases=$(set_cases "$source" "$sha")
  [ -n "$cases" ] || die "the source $source has no cases"
  local id repo srcdir csha prompt n=0 turn=0 pair_k=0 pair_first='' pair_dir total live_repo='' live_case='' runid run_dir pws nws ph nh hookbin tab_id p_pane n_pane reuse=${ARENA_REUSE:-1}
  total=$(printf '%s\n' "$cases" | awk -F'\t' -v o="$only" 'BEGIN{split(o,a,","); for(i in a) w[a[i]]=1} o=="" || ($1 in w) {c++} END{print c+0}')
  say "set $source: $total case(s), pixel binary ${bin_hash:0:12}${extra:+, brief extra: $extra}; output $SET_OUT"
  set_unlock_stale
  prewarm_bases "$cases" "$only" "$bin" "$bin_hash" &
  SET_PREWARM=$!
  # --here: the caller tab is shared — close every leftover arena pair from
  # earlier runs or a killed set before the first case opens its own.
  [ "$here" = 1 ] && close_all_arena_panes
  while IFS=$'\t' read -r id repo srcdir csha prompt <&3; do
    [ -n "$id" ] || continue
    if [ -n "$only" ] && ! printf ',%s,' "$only" | grep -q ",$id,"; then continue; fi
    n=$((n + 1)); SET_CASE=$id; tm_start=$(date +%s)
    local reused=0
    if [ "$reuse" = 1 ] && [ "$repo-$csha" = "$live_repo" ]; then reused=1; fi
    # --here keeps the finished case's panes until the next one opens its pair;
    # a pair for this same case left by an earlier run is always stale.
    if [ "$reused" = 0 ]; then
      [ "${ARENA_CLEAR:-1}" = 1 ] || [ -z "$live_repo" ] || session_pair_write "$source" "$pair_k" "$pair_first" "$pws" "$nws"
      [ -z "$live_case" ] || close_case_tab "$live_case"
      [ "$here" != 1 ] || close_case_tab "$id"
    fi
    say "[$n/$total] $id"
    [ -d "$srcdir/.git" ] || die "no source clone for $id: $srcdir (bench: scripts/bench/brief-bench.py prepare)"
    if [ "$reused" = 0 ]; then
    make_panes "$id" "$SCRATCH" "$SCRATCH" "$here" --no-focus
    p_pane=$P_PANE; n_pane=$N_PANE; tab_id=$TAB_ID
    [ -n "$p_pane" ] && [ -n "$n_pane" ] || die 'could not create the two panes'
    titles "$p_pane" "$n_pane" "$id"
    hd pane run "$p_pane" "echo 'arena: $id pixel arm: indexing $repo ...'" >/dev/null
    hd pane run "$n_pane" "echo 'arena: $id no-pixel arm: waiting for the clone ...'" >/dev/null
    set_base "$repo" "$srcdir" "$csha" "$bin" "$bin_hash"
    tm_base=$(date +%s)
    runid="$id-$(date -u +%Y%m%dT%H%M%SZ)"; run_dir="$SCRATCH/runs/$runid"; pair_dir=$run_dir; turn=1; pair_k=$((pair_k + 1)); pair_first=$id
    pws="$run_dir/pixel/ws"; nws="$run_dir/no-pixel/ws"; ph="$run_dir/pixel/codex-home"; nh="$run_dir/no-pixel/codex-home"
    mkdir -p "$run_dir/pixel" "$run_dir/no-pixel"
    # Fresh copy-on-write clones per case: the indexed one for the pixel arm, a bare one for the other.
    rm -rf "$pws" "$nws"
    cp -c -R "$SET_BASE/pixel/ws" "$pws"; cp -c -R "$SET_BASE/no-pixel/ws" "$nws"
    (cd "$pws" && "$bin" prepare . >/dev/null 2>&1) || true
    settle_daemon "$bin" "$pws"
    tm_clone=$(date +%s)
    # The hook binary: the pinned one, or the wrapper that hydrates the brief with $extra.
    hookbin=$bin
    if [ -n "$extra" ]; then
      mkdir -p "$run_dir/pixel/hookwrap"
      cp "$SELF_DIR/hook-wrap.py" "$run_dir/pixel/hookwrap/pixel"; chmod +x "$run_dir/pixel/hookwrap/pixel"
      jq -n --arg real "$bin" --arg cmd "$extra" '{real:$real, extra:[$cmd]}' >"$run_dir/pixel/hookwrap/hook-wrap.json"
      hookbin="$run_dir/pixel/hookwrap/pixel"
    fi
    if [ "$AGENT" = claude ]; then
      make_claude_home "$ph" "$hookbin" "$parts"; make_claude_home "$nh" "" 1
    elif [ "$AGENT" = devin ]; then
      make_devin_home "$ph" "$hookbin" "$parts"; make_devin_home "$nh" "" 1
    else
      make_codex_home "$ph" "$pws" ""; write_part_hooks "$ph" "$hookbin" "$parts"
      make_codex_home "$nh" "$nws" ""
    fi
    mkdir -p "$run_dir/pixel/shim" "$run_dir/no-pixel/shim"
    printf '#!/bin/sh\nexec %s "$@"\n' "'$bin'" >"$run_dir/pixel/shim/pixel"
    printf '#!/bin/sh\necho "pixel: command not found" >&2\nexit 127\n' >"$run_dir/no-pixel/shim/pixel"
    chmod +x "$run_dir/pixel/shim/pixel" "$run_dir/no-pixel/shim/pixel"
    hd pane run "$p_pane" "cd '$pws'" >/dev/null; hd pane run "$n_pane" "cd '$nws'" >/dev/null
    # run.json keeps a digest of the prompt, never the prompt text.
    jq -n --arg id "$runid" --arg tab "$tab_id" --arg pp "$p_pane" --arg np "$n_pane" --arg ph "$ph" --arg nh "$nh" \
      --arg name "$id" --arg digest "$(printf '%s' "$prompt" | shasum -a 256 | cut -c1-16)" --arg sha "$csha" --arg bh "$bin_hash" \
      '{run:$id,tab:$tab,pixel_pane:$pp,no_pixel_pane:$np,pixel_home:$ph,no_pixel_home:$nh,case:0,name:$name,prompt:("sha256:"+$digest),commit:$sha,pixel_binary_sha256:$bh}' >"$run_dir/run.json"
    run_json_extend "$run_dir" "$pws" "$nws"
    tm_panes=$(date +%s)
    sleep 2
    hd pane run "$p_pane" "$(agent_launch pixel "$pws" "$ph" "$run_dir/pixel/shim")" >/dev/null
    hd pane run "$n_pane" "$(agent_launch no-pixel "$nws" "$nh" "$run_dir/no-pixel/shim")" >/dev/null
    wait_ready "$p_pane" || die "pixel arm never became ready (pane $p_pane)"
    wait_ready "$n_pane" || die "no-pixel arm never became ready (pane $n_pane)"
    review_hooks_if_asked "$p_pane"
    wait_ready "$p_pane" || die "pixel arm not ready after hook review"
    hd agent rename "$p_pane" "$PREFIX-$id-pixel" >/dev/null 2>&1 || true
    hd agent rename "$n_pane" "$PREFIX-$id-no-pixel" >/dev/null 2>&1 || true
    titles "$p_pane" "$n_pane" "$id"
    live_repo="$repo-$csha"; live_case=$id
    else
      say "$id: reusing the panes (/clear)"
      runid="$id-$(date -u +%Y%m%dT%H%M%SZ)"; run_dir="$SCRATCH/runs/$runid"; turn=$((turn + 1))
      mkdir -p "$run_dir/pixel" "$run_dir/no-pixel"
      jq --arg id "$runid" --arg name "$id" --arg digest "$(printf '%s' "$prompt" | shasum -a 256 | cut -c1-16)" \
        '.run = $id | .name = $name | .prompt = ("sha256:" + $digest)' "$pair_dir/run.json" >"$run_dir/run.json"
      tm_base=$(date +%s); tm_clone=$tm_base
      if [ "${ARENA_CLEAR:-1}" = 1 ]; then
        hd agent prompt "$p_pane" "/clear" >/dev/null 2>&1 || true
        hd agent prompt "$n_pane" "/clear" >/dev/null 2>&1 || true
        sleep 2
      fi
      wait_ready "$p_pane" || die "pixel arm not ready after /clear (pane $p_pane)"
      wait_ready "$n_pane" || die "no-pixel arm not ready after /clear (pane $n_pane)"
      titles "$p_pane" "$n_pane" "$id"; live_case=$id
    fi
    local t0 t1 rc_p=0 rc_n=0
    t0=$(date +%s); tm_ready=$t0
    local first=pixel pid_p pid_n nprompt=$prompt
    if [ "$cross" = 1 ]; then
      nprompt=$(printf '%s\n' "$cases" | awk -F'\t' -v r="$repo" -v i="$id" '$2==r{id[++k]=$1; p[k]=$5; if($1==i) at=k} END{print p[at%k+1]}')
    fi
    [ $((n % 2)) = 0 ] && first=no-pixel
    if [ "$first" = pixel ]; then
      hd agent prompt "$p_pane" "$prompt" --wait --timeout $((timeout * 1000)) >"$run_dir/pixel/prompt.json" 2>&1 & pid_p=$!; sleep 1
      hd agent prompt "$n_pane" "$nprompt" --wait --timeout $((timeout * 1000)) >"$run_dir/no-pixel/prompt.json" 2>&1 & pid_n=$!
    else
      hd agent prompt "$n_pane" "$nprompt" --wait --timeout $((timeout * 1000)) >"$run_dir/no-pixel/prompt.json" 2>&1 & pid_n=$!; sleep 1
      hd agent prompt "$p_pane" "$prompt" --wait --timeout $((timeout * 1000)) >"$run_dir/pixel/prompt.json" 2>&1 & pid_p=$!
    fi
    # The review panel opens when the prompt actually runs the hooks: patrol.
    ( for _ in $(seq 60); do review_hooks_if_asked "$p_pane"; sleep 3; done ) & local pid_t=$!
    SET_PIDS="$pid_p $pid_n $pid_t"
    wait "$pid_p" || rc_p=$?
    wait "$pid_n" || rc_n=$?
    kill "$pid_t" 2>/dev/null || true; wait "$pid_t" 2>/dev/null || true
    SET_PIDS=''
    t1=$(date +%s)
    titles "$p_pane" "$n_pane" "$id"
    collect_receipt "$run_dir" "$rc_p" "$rc_n" "$((t1 - t0))"
    local bytes pc nc base=null
    if [ "$reused" = 1 ]; then bytes=$(jq '(.arms.pixel.brief // "") | utf8bytelength' "$run_dir/receipt.json")
    else bytes=$(brief_bytes_of "$pws"); fi
    cp "$run_dir/receipt.json" "$SET_OUT/$id.receipt.json" 2>/dev/null || true
    pc=$(jq '.arms.pixel.tool_calls | length' "$run_dir/receipt.json")
    nc=$(jq '.arms["no-pixel"].tool_calls | length' "$run_dir/receipt.json")
    if [ "${ARENA_CLEAR:-1}" = 0 ]; then
      local tp tn
      tp=$(python3 -I "$SELF_DIR/session-digest.py" "$(newest_transcript "$pws")" --turn "$turn" --hide-prompt)
      tn=$(python3 -I "$SELF_DIR/session-digest.py" "$(newest_transcript "$nws")" --turn "$turn" --hide-prompt)
      pc=$(jq '(.tool_calls // []) | length' <<<"$tp"); nc=$(jq '(.tool_calls // []) | length' <<<"$tn")
      bytes=$(jq '[(.briefs // [])[] | utf8bytelength] | add // 0' <<<"$tp")
    fi
    local timings
    timings=$(jq -nc --argjson a "$((tm_base - tm_start))" --argjson b "$((tm_clone - tm_base))" --argjson c "$((tm_ready - tm_clone))" --argjson d "$((t1 - t0))" \
      '{index_s:$a, clone_settle_s:$b, spawn_s:$c, prompt_s:$d}')
    local arm_s; arm_s=$(jq -c '{pixel_s:.arms.pixel.codex_seconds, no_pixel_s:.arms["no-pixel"].codex_seconds}' "$run_dir/receipt.json")
    jq -nc --arg id "$id" --argjson pc "$pc" --argjson nc "$nc" --argjson b "$bytes" --arg run "$runid" --argjson tm "$timings" --arg first "$first" --argjson arm "$arm_s" \
      '{id:$id, pixel_calls:$pc, no_pixel_calls:$nc, brief_bytes:$b, run:$run, timings:$tm, first:$first, arm_seconds:$arm}' >>"$SET_OUT/results.jsonl"
    say "$id: no-pixel $nc calls, pixel $pc calls, brief $bytes bytes; seconds $timings"
    if [ -s "$run_dir/brief.md" ]; then
      say "----- $id pixel brief (injected into the with-pixel arm) -----"
      cat "$run_dir/brief.md"
      say "----- end of $id pixel brief -----"
    else
      say "$id: NO brief delivered ($run_dir/brief.md empty)"
    fi
    # --here leaves the pair open; the next case or the next relaunch closes it.
    [ "$here" = 1 ] || [ "$reuse" = 1 ] || close_case_tab "$id"
    SET_CASE=''
  done 3<<<"$cases"
  [ "$here" = 1 ] || [ -z "$live_case" ] || close_case_tab "$live_case"
  local rr=''
  if [ "${ARENA_CLEAR:-1}" = 0 ] && [ -n "$pws" ]; then
    session_pair_write "$source" "$pair_k" "$pair_first" "$pws" "$nws"
    rr=$(cd "$SELF_DIR/../../.." && pwd)/.arena-reports/${SET_OUT##*/}
    mkdir -p "$rr" && cp "$SET_OUT/session"/* "$rr"/ || true
    say "session: $rr/report.md (every brief and tool call, turn by turn)"
  fi
  rm -f "$SET_PID_FILE"
  trap - INT TERM
  set_table "$SET_OUT"
  [ -z "$rr" ] || cp "$SET_OUT/table.md" "$rr"/ 2>/dev/null || true
}

# ---------------------------------------------------------------- main

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  case "${1:-}" in
    list) cmd_list ;;
    run) shift; cmd_run "$@" ;;
    run-set) shift; cmd_run_set "$@" ;;
    status) cmd_status ;;
    receipt) shift; cmd_receipt "$@" ;;
    abort) shift; cmd_abort "$@" ;;
    -h|--help|help|'') usage ;;
    *) usage >&2; exit 2 ;;
  esac
fi
