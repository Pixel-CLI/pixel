#!/usr/bin/env bash
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

# Run one Codex control and one Pixel-enabled Codex candidate side by side.
# Both arms start from detached worktrees at the same committed revision.
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: scripts/codex-pixel-ab.sh --prompt TEXT [options]

Open two tmux panes: a clean Codex control and Codex with the candidate Pixel
install. Both use isolated worktrees at the same committed source revision.

Options:
  --prompt TEXT       Prompt to send to both arms.
  --prompt-file PATH  Read the prompt from a file instead.
  --repo PATH         Git repository to clone into worktrees (default: cwd).
  --base REV          Committed revision for both arms (default: HEAD).
  --pixel-bin PATH    Candidate Pixel binary (default: PIXEL_BIN, then pixel).
  --pixel-policy MODE Pixel-arm policy: advisory, enforce, or classify (default: advisory).
  --classify-min-confidence N
                       Minimum winning probability for classify to enforce Pixel (default: 0.60).
  --session NAME      tmux session name (default: pixel-ab-<timestamp>).
  --timeout SECONDS   Mark unfinished arms timed out after this time (default: 900).
  --detach            Do not attach; useful for automated wiring tests.
  -h, --help          Show this help.

The control disables Codex user configuration, repository rules and hooks. The
candidate receives only a project-local Pixel install in its disposable
worktree. Both arms run unattended and their JSONL transcripts are saved. The
panes replace themselves with the shared report when both arms finish. Existing
Codex login credentials remain in use. The command can let the agents edit
their disposable worktrees; never point it at uncommitted work.
EOF
}

die() { printf 'codex-pixel-ab: %s\n' "$*" >&2; exit 1; }

PROMPT=''
PROMPT_FILE=''
REPO=''
BASE='HEAD'
PIXEL_BIN="${PIXEL_BIN:-}"
PIXEL_POLICY_MODE=advisory
CLASSIFY_MIN_CONFIDENCE=0.60
SESSION="pixel-ab-$(date +%Y%m%d-%H%M%S)"
TIMEOUT=900
DETACH=0

while [ "$#" -gt 0 ]; do
  case "$1" in
    --prompt) PROMPT=${2:?--prompt needs text}; shift 2 ;;
    --prompt-file) PROMPT_FILE=${2:?--prompt-file needs a path}; shift 2 ;;
    --repo) REPO=${2:?--repo needs a path}; shift 2 ;;
    --base) BASE=${2:?--base needs a revision}; shift 2 ;;
    --pixel-bin) PIXEL_BIN=${2:?--pixel-bin needs a path}; shift 2 ;;
    --pixel-policy) PIXEL_POLICY_MODE=${2:?--pixel-policy needs advisory or enforce}; shift 2 ;;
    --classify-min-confidence) CLASSIFY_MIN_CONFIDENCE=${2:?--classify-min-confidence needs a number}; shift 2 ;;
    --session) SESSION=${2:?--session needs a name}; shift 2 ;;
    --timeout) TIMEOUT=${2:?--timeout needs seconds}; shift 2 ;;
    --detach) DETACH=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) die "unknown option: $1" ;;
  esac
done

[ -z "$PROMPT" ] || [ -z "$PROMPT_FILE" ] || die 'use --prompt or --prompt-file, not both'
if [ -n "$PROMPT_FILE" ]; then
  [ -f "$PROMPT_FILE" ] || die "prompt file not found: $PROMPT_FILE"
  PROMPT=$(<"$PROMPT_FILE")
fi
[ -n "$PROMPT" ] || die 'a prompt is required'
case "$TIMEOUT" in *[!0-9]*|'') die '--timeout must be a positive integer' ;; esac
[ "$TIMEOUT" -gt 0 ] || die '--timeout must be positive'
case "$PIXEL_POLICY_MODE" in advisory|enforce|classify) ;; *) die '--pixel-policy must be advisory, enforce, or classify' ;; esac
awk -v v="$CLASSIFY_MIN_CONFIDENCE" 'BEGIN { exit !(v ~ /^[0-9]*\.?[0-9]+$/ && v+0 >= 0 && v+0 <= 1) }' || die '--classify-min-confidence must be a number between 0 and 1'

REPO=${REPO:-$PWD}
REPO=$(cd "$REPO" && pwd) || die "repository not found: $REPO"
git -C "$REPO" rev-parse --is-inside-work-tree >/dev/null 2>&1 || die "$REPO is not a git repository"
BASE_SHA=$(git -C "$REPO" rev-parse --verify "$BASE^{commit}") || die "invalid base revision: $BASE"
command -v tmux >/dev/null 2>&1 || die 'tmux is not on PATH'
command -v codex >/dev/null 2>&1 || die 'codex is not on PATH'
if [ -z "$PIXEL_BIN" ]; then PIXEL_BIN=$(command -v pixel || true); fi
[ -n "$PIXEL_BIN" ] && [ -x "$PIXEL_BIN" ] || die 'candidate Pixel binary not found; pass --pixel-bin'
PIXEL_BIN=$(cd "$(dirname "$PIXEL_BIN")" && pwd)/$(basename "$PIXEL_BIN")
tmux has-session -t "$SESSION" 2>/dev/null && die "tmux session already exists: $SESSION"

RUN_ROOT="$REPO/target/codex-pixel-ab"
mkdir -p "$RUN_ROOT"
RUN_DIR=$(mktemp -d "$RUN_ROOT/run-XXXXXX")
RAW_DIR="$RUN_DIR/raw"
PIXEL_DIR="$RUN_DIR/pixel"
PROMPT_SHA=$(printf %s "$PROMPT" | shasum -a 256 | awk '{print $1}')
printf '%s' "$PROMPT" > "$RUN_DIR/prompt.txt"
if [ -n "$(git -C "$REPO" status --porcelain)" ]; then
  printf 'codex-pixel-ab: source has local changes; both arms use committed %s only\n' "$BASE_SHA" >&2
fi

CLASSIFIED_ROUTE=static
CLASSIFIED_PROBABILITY=n/a
EFFECTIVE_PIXEL_POLICY=$PIXEL_POLICY_MODE
if [ "$PIXEL_POLICY_MODE" = classify ]; then
  if ! "$PIXEL_BIN" classify "$PROMPT" --context "Route this coding-agent prompt. Choose pixel only when a scoped Pixel code, history, or impact retrieval would materially improve the answer; do not choose it merely because Pixel is mentioned." --label none --criterion 'none=Answer without repository retrieval; Pixel offers no material benefit.' --label pixel --criterion 'pixel=A scoped Pixel code, history, or impact retrieval would materially improve the answer.' --label native --criterion 'native=A repository lookup is useful, but Pixel cannot express it.' --json --metrics off > "$RUN_DIR/classify.json" 2> "$RUN_DIR/classify.stderr"; then
    die "classify failed; inspect $RUN_DIR/classify.stderr"
  fi
  CLASSIFY_RESULT=$(CLASSIFY_PATH="$RUN_DIR/classify.json" bun -e 'const { readFileSync } = require("node:fs"); const value = JSON.parse(readFileSync(process.env.CLASSIFY_PATH, "utf8")); const route = value?.predicted; const probability = value?.probs?.[route]; if (!["none", "pixel", "native"].includes(route) || typeof probability !== "number") process.exit(1); console.log(`${route}\t${probability}`);') || die "classify returned an invalid route; inspect $RUN_DIR/classify.json"
  IFS=$'\t' read -r CLASSIFIED_ROUTE CLASSIFIED_PROBABILITY <<< "$CLASSIFY_RESULT"
  if [ "$CLASSIFIED_ROUTE" = pixel ] && bun -e 'process.exit(Number(process.argv[1]) >= Number(process.argv[2]) ? 0 : 1)' "$CLASSIFIED_PROBABILITY" "$CLASSIFY_MIN_CONFIDENCE"; then
    EFFECTIVE_PIXEL_POLICY=enforce
  else
    EFFECTIVE_PIXEL_POLICY=off
  fi
fi

cleanup() {
  # Stop the arms before removing their worktrees: a runner still writing in
  # a removed worktree keeps consuming against a deleted checkout.
  tmux kill-session -t "$SESSION" >/dev/null 2>&1 || true
  for _ in 1 2 3 4 5; do
    tmux has-session -t "$SESSION" 2>/dev/null || break
    sleep 1
  done
  git -C "$REPO" worktree remove --force "$RAW_DIR" >/dev/null 2>&1 || true
  git -C "$REPO" worktree remove --force "$PIXEL_DIR" >/dev/null 2>&1 || true
}
trap cleanup ERR INT TERM EXIT
git -C "$REPO" worktree add --detach "$RAW_DIR" "$BASE_SHA" >/dev/null
git -C "$REPO" worktree add --detach "$PIXEL_DIR" "$BASE_SHA" >/dev/null
[ ! -e "$RAW_DIR/.codex" ] || die 'base contains .codex; cannot establish a clean raw control'
# The raw arm is the non-Pixel control: --ignore-rules skips exec-policy rules
# only, so Pixel-first agent instructions and the pixel binary itself must be
# out of reach in the raw worktree. Strip the rule files Codex/agent would
# read and shadow `pixel` on PATH with a failing stub.
rm -rf "$RAW_DIR/.claude" "$RAW_DIR/.agents" "$RAW_DIR/.cursor" "$RAW_DIR/.pixel"
rm -f "$RAW_DIR/AGENTS.md" "$RAW_DIR/CLAUDE.md"
RAW_BIN="$RUN_DIR/raw-bin"
mkdir -p "$RAW_BIN"
printf '#!/usr/bin/env bash\nprintf "pixel: command not available in the raw control arm\\n" >&2\nexit 127\n' > "$RAW_BIN/pixel"
chmod +x "$RAW_BIN/pixel"

if [ "$EFFECTIVE_PIXEL_POLICY" != off ]; then
  "$PIXEL_BIN" install --repo "$PIXEL_DIR" >/dev/null
  if [ "$EFFECTIVE_PIXEL_POLICY" = enforce ]; then
    "$PIXEL_BIN" build-index "$PIXEL_DIR" >/dev/null
  fi
fi

write_runner() {
  local arm=$1 dir=$2 command=$3
  local runner="$RUN_DIR/$arm-runner.sh"
  {
    printf '#!/usr/bin/env bash\nset -uo pipefail\n'
    printf 'started=$(date +%%s)\n'
    printf 'set +e\n'
    printf '%s > %q 2>&1\n' "$command" "$RUN_DIR/$arm.jsonl"
    printf 'exit_code=$?\nfinished=$(date +%%s)\n'
    printf 'printf "exit=%%s\\nstarted=%%s\\nfinished=%%s\\n" "$exit_code" "$started" "$finished" > %q\n' "$RUN_DIR/$arm.meta"
    printf 'printf "\\n--- %s complete; report follows ---\\n"\n' "$arm"
    printf 'while [ ! -f %q ]; do sleep 1; done\n' "$RUN_DIR/report.md"
    printf 'cat %q\n' "$RUN_DIR/report.md"
    printf 'exec "${SHELL:-/bin/bash}"\n'
  } > "$runner"
  chmod +x "$runner"
}

q() { printf '%q' "$1"; }
PROMPT_Q=$(q "$PROMPT")
RAW_Q=$(q "$RAW_DIR")
PIXEL_Q=$(q "$PIXEL_DIR")
ANSWER_RAW_Q=$(q "$RUN_DIR/raw-answer.md")
ANSWER_PIXEL_Q=$(q "$RUN_DIR/pixel-answer.md")
PIXEL_CONFIG="projects.\"$PIXEL_DIR\".trust_level=\"trusted\""
RAW_PATH_Q=$(q "$RAW_BIN:$PATH")
RAW_COMMAND="env PATH=$RAW_PATH_Q -u PIXEL_BIN -u PIXEL_POLICY codex exec --json --ephemeral --ignore-user-config --ignore-rules -c features.hooks=false --approve-for-me -C $RAW_Q --output-last-message $ANSWER_RAW_Q $PROMPT_Q"
if [ "$EFFECTIVE_PIXEL_POLICY" = off ]; then
  PIXEL_COMMAND="env -u PIXEL_BIN -u PIXEL_POLICY codex exec --json --ephemeral --ignore-user-config --ignore-rules -c features.hooks=false --approve-for-me -C $PIXEL_Q --output-last-message $ANSWER_PIXEL_Q $PROMPT_Q"
else
  PIXEL_COMMAND="env PIXEL_POLICY=$(q "$EFFECTIVE_PIXEL_POLICY") codex exec --json --ephemeral --ignore-user-config --dangerously-bypass-hook-trust -c features.hooks=true -c $(q "$PIXEL_CONFIG") --approve-for-me -C $PIXEL_Q --output-last-message $ANSWER_PIXEL_Q $PROMPT_Q"
fi
write_runner raw "$RAW_DIR" "$RAW_COMMAND"
write_runner pixel "$PIXEL_DIR" "$PIXEL_COMMAND"

tmux new-session -d -s "$SESSION" -n codex -c "$RAW_DIR" "bash $(q "$RUN_DIR/raw-runner.sh")"
tmux split-window -h -t "$SESSION":0 -c "$PIXEL_DIR" "bash $(q "$RUN_DIR/pixel-runner.sh")"
tmux select-layout -t "$SESSION":0 even-horizontal
# The arms own the worktrees from here on; only signal exits still clean up.
trap - EXIT

report() {
  local now raw_exit pixel_exit raw_started raw_finished pixel_started pixel_finished raw_wall pixel_wall raw_calls pixel_calls
  now=$(date +%s)
  read_meta() { [ -f "$1" ] && awk -F= -v key="$2" '$1 == key {print $2}' "$1" || true; }
  raw_exit=$(read_meta "$RUN_DIR/raw.meta" exit); pixel_exit=$(read_meta "$RUN_DIR/pixel.meta" exit)
  raw_started=$(read_meta "$RUN_DIR/raw.meta" started); raw_finished=$(read_meta "$RUN_DIR/raw.meta" finished)
  pixel_started=$(read_meta "$RUN_DIR/pixel.meta" started); pixel_finished=$(read_meta "$RUN_DIR/pixel.meta" finished)
  raw_wall=$(( ${raw_finished:-$now} - ${raw_started:-$now} ))
  pixel_wall=$(( ${pixel_finished:-$now} - ${pixel_started:-$now} ))
  raw_calls=$(rg -c '"type":"item.started".*"command":.*pixel (find-code|search-content|impact|scope-task)' "$RUN_DIR/raw.jsonl" 2>/dev/null || true)
  pixel_calls=$(rg -c '"type":"item.started".*"command":.*pixel (find-code|search-content|impact|scope-task)' "$RUN_DIR/pixel.jsonl" 2>/dev/null || true)
  local raw_status=${raw_exit:-timeout} pixel_status=${pixel_exit:-timeout}
  [ "${RAW_TIMED_OUT:-0}" -eq 0 ] || raw_status="timeout (exit ${raw_exit:-none})"
  [ "${PIXEL_TIMED_OUT:-0}" -eq 0 ] || pixel_status="timeout (exit ${pixel_exit:-none})"
  cat > "$RUN_DIR/report.md" <<EOF
# Codex / Pixel A/B report

| Arm | Exit | Wall | Pixel retrieval commands | Last answer |
| --- | ---: | ---: | ---: | --- |
| Raw Codex | ${raw_status} | ${raw_wall}s | ${raw_calls:-0} | [raw-answer.md](raw-answer.md) |
| Codex + Pixel | ${pixel_status} | ${pixel_wall}s | ${pixel_calls:-0} | [pixel-answer.md](pixel-answer.md) |

- Base: \`${BASE_SHA}\`
- Prompt SHA-256: \`${PROMPT_SHA}\`
- Candidate Pixel: \`${PIXEL_BIN}\`
- Requested Pixel policy: \`${PIXEL_POLICY_MODE}\`
- Effective Pixel policy: \`${EFFECTIVE_PIXEL_POLICY}\`
- Classifier route / winning probability: \`${CLASSIFIED_ROUTE}\` / \`${CLASSIFIED_PROBABILITY}\`
- Classifier artifact: [classify.json](classify.json)
- Artifacts: \`${RUN_DIR}\`

Pixel retrieval commands are a transcript indicator, not a quality score. Read
the two answers and diffs before treating one run as evidence of a benefit.
EOF
}

deadline=$(( $(date +%s) + TIMEOUT ))
RAW_TIMED_OUT=0
PIXEL_TIMED_OUT=0
while [ ! -f "$RUN_DIR/raw.meta" ] || [ ! -f "$RUN_DIR/pixel.meta" ]; do
  [ "$(date +%s)" -lt "$deadline" ] || break
  sleep 1
done
# Deadline hit: interrupt each arm still running so no Codex keeps consuming
# past the reported timeout, then give its runner a moment to record the
# exit it ended with. Pane 0 is the raw arm, pane 1 the Pixel arm.
if [ ! -f "$RUN_DIR/raw.meta" ] || [ ! -f "$RUN_DIR/pixel.meta" ]; then
  # Record which arms missed the deadline before interrupting them: an arm
  # that writes its meta after C-c still ran past the requested timeout, and
  # the report must say so rather than showing its post-interrupt exit code.
  [ -f "$RUN_DIR/raw.meta" ] || { RAW_TIMED_OUT=1; tmux send-keys -t "$SESSION":0.0 C-c || true; }
  [ -f "$RUN_DIR/pixel.meta" ] || { PIXEL_TIMED_OUT=1; tmux send-keys -t "$SESSION":0.1 C-c || true; }
  grace=$(( $(date +%s) + 5 ))
  while [ ! -f "$RUN_DIR/raw.meta" ] || [ ! -f "$RUN_DIR/pixel.meta" ]; do
    [ "$(date +%s)" -lt "$grace" ] || break
    sleep 1
  done
fi
report
printf 'A/B report: %s\n' "$RUN_DIR/report.md"
printf 'tmux session: %s\n' "$SESSION"
trap - ERR INT TERM
if [ "$DETACH" -eq 0 ]; then exec tmux attach-session -t "$SESSION"; fi
