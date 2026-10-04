#!/usr/bin/env bash
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

# Records the agent runs the AgentDemo composition replays.
#
# Both arms run the same model, at the same effort, on the same task, in a
# throwaway worktree of a pinned ref with docs/motion removed (so no agent
# can read the demo's own traces or this script). Both start from the same
# bare configuration: no settings sources (so no CLAUDE.md, rules, hooks or
# permissions from any settings file), no skills, no MCP server, no
# sub-agents and no editing tools, and both are told to answer in English.
#
# The only difference: the `pixel` arm gets the hooks `pixel install` writes
# for Claude Code (SessionStart injects the agent prompt, UserPromptSubmit
# adds the task packet, PostToolUse records metrics), passed with
# --settings, so it measures Pixel as a user installs it.
#
# Usage: [ARMS="vanilla pixel"] [PAR=n] [REF=v0.5.0] [EFFORT=medium] \
#        [TASK="..."] [CANDIDATE_PROMPT=file] [CANDIDATE_BIN=path/to/pixel] \
#          scripts/record-demo.sh <out-dir> [reps] [model]
# Both arms of a rep always start together; PAR caps how many reps run at
# once (default: 3), so a rate limit or a busy machine hits both arms alike.
# Keep it low: on an 8-core laptop 22 simultaneous sessions saturate the CPU,
# the task-packet query of the prompt-submit hook then takes 500-900 ms
# instead of ~150 ms, and every run past its 750 ms deadline starts without a
# packet. packets.txt records, per Pixel run, whether its packet was
# delivered (the session is in its copy's .pixel/task-runtime.json).
#
# TASK replaces the default task (the English-answer instruction still
# follows it). CANDIDATE_PROMPT adds a `candidate` arm to compare two agent
# prompts, one variable at a time: `candidate` gets the same hooks as
# `pixel`, but its SessionStart hook runs with HOME pointed at a directory
# holding that file as .local/share/pixel/agent-prompt.md, the only file the
# hook reads from HOME. The `pixel` arm then goes through the same mechanism
# with a copy of the installed prompt, so the two arms differ by the prompt
# text alone. Use ARMS="pixel candidate" for that comparison.
#
# CANDIDATE_BIN compares two Pixel binaries instead: the `candidate` arm's
# hooks, its index and the `pixel` its agent runs are that binary (first on
# its PATH); every other input stays the installed one's. Setting both
# CANDIDATE_* options changes two variables at once, and meta.txt says so.
#
# The repository is a fresh one holding REF's history only (objects are
# borrowed from the source repository through alternates): no branch, tag or
# later commit of the source is reachable, so `git log --all` or a history
# search cannot find the change a task asks for. Every run then works in its
# own copy of it (an APFS clone where available), indexed with its arm's
# binary and served by its own daemon: the daemon answers one request at a
# time, so 22 sessions sharing one queued their task-packet queries past the
# prompt-submit hook's 750 ms deadline and a third of the runs got no packet.
# Every run also gets its own PIXEL_SESSION_ID, so one run's Pixel calls
# never feed another run's repeated-call notes.
# Writes <arm>-<rep>.jsonl: one stream-json event per line, each wrapped as
# {"t": <ms since epoch when the line arrived>, "e": <event>}.
set -euo pipefail

out=${1:?out dir}
reps=${2:-3}
model=${3:-opus}
effort=${EFFORT:-medium}
ref=${REF:-v0.5.0}
source_repo=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
pixel_bin=$(command -v pixel)
prompt_file=$HOME/.local/share/pixel/agent-prompt.md
task=${TASK:-'Task: retry a leased push when the remote branch moved. Before anyone edits anything, find the files that would need to change and list them, most important first, with one line each on why. Do not edit any file.'}
candidate=${CANDIDATE_PROMPT:-}
if [ -n "$candidate" ]; then candidate=$(cd "$(dirname "$candidate")" && pwd)/$(basename "$candidate"); fi
cand_bin=${CANDIDATE_BIN:-}
if [ -n "$cand_bin" ]; then cand_bin=$(cd "$(dirname "$cand_bin")" && pwd)/$(basename "$cand_bin"); fi
english='Always write your final answer in English.'

mkdir -p "$out"
out=$(cd "$out" && pwd)

# The repository both arms work in, outside the source tree, indexed before
# any clock starts: REF's history only, see the header.
work=$(mktemp -d)
repo="$work/pixel"
stop_daemons() {
  for d in "$work"/*/; do
    "$pixel_bin" daemon stop "$d" >/dev/null 2>&1 || true
    if [ -n "$cand_bin" ]; then "$cand_bin" daemon stop "$d" >/dev/null 2>&1 || true; fi
  done
}
trap 'stop_daemons; rm -rf "$work"' EXIT
ref_commit=$(git -C "$source_repo" rev-parse "$ref^{commit}")
git init --quiet "$repo"
echo "$(git -C "$source_repo" rev-parse --path-format=absolute --git-common-dir)/objects" > "$repo/.git/objects/info/alternates"
git -C "$repo" update-ref refs/heads/main "$ref_commit"
git -C "$repo" checkout --quiet --force main
rm -rf "$repo/docs/motion"
git -C "$repo" add -A
# An older REF has no docs/motion: nothing to drop, nothing to commit.
git -C "$repo" diff --cached --quiet ||
  git -C "$repo" -c user.name=demo -c user.email=demo@localhost commit --quiet -m "demo: drop the demo's own sources"
(cd "$repo" && pixel prepare-repo . >/dev/null 2>&1)

# The hooks of a Pixel arm; $1 prefixes the SessionStart command (an
# environment assignment, or nothing), $2 is the binary.
settings_for() {
  local bin=${2:-$pixel_bin}
  cat <<JSON
{"hooks": {
  "SessionStart": [{"hooks": [{"type": "command", "command": "$1'$bin' run-hook session-start"}]}],
  "UserPromptSubmit": [{"hooks": [{"type": "command", "command": "'$bin' run-hook prompt-submit --provider claude"}]}],
  "PostToolUse": [{"matcher": "Edit", "hooks": [{"type": "command", "command": "'$bin' run-hook post-tool-use --provider claude"}]}]
}}
JSON
}
# A HOME for the SessionStart hook that holds one agent prompt.
prompt_home() {
  mkdir -p "$work/home-$1/.local/share/pixel"
  cp "$2" "$work/home-$1/.local/share/pixel/agent-prompt.md"
  echo "HOME='$work/home-$1' "
}
settings=$(settings_for "")
candidate_settings=$(settings_for "" "${cand_bin:-$pixel_bin}")
if [ -n "$candidate" ]; then
  settings=$(settings_for "$(prompt_home pixel "$prompt_file")")
  candidate_settings=$(settings_for "$(prompt_home candidate "$candidate")" "${cand_bin:-$pixel_bin}")
fi
cand_path=$PATH
if [ -n "$cand_bin" ]; then
  mkdir -p "$work/bin-candidate"
  ln -s "$cand_bin" "$work/bin-candidate/pixel"
  cand_path="$work/bin-candidate:$PATH"
fi

# One copy of the prepared repository per run, indexed by its arm's binary
# before any clock starts; the template's own daemon is stopped.
"$pixel_bin" daemon stop "$repo" >/dev/null 2>&1 || true
for rep in $(seq 1 "$reps"); do
  for arm in ${ARMS:-vanilla pixel}; do
    cp -c -R "$repo" "$work/$arm-$rep" 2>/dev/null || cp -R "$repo" "$work/$arm-$rep"
    case $arm in
      pixel) bin=$pixel_bin ;;
      candidate) bin=${cand_bin:-$pixel_bin} ;;
      *) continue ;;
    esac
    # Index, then one query so the daemon has the index loaded when the
    # hook asks for the task packet: a cold daemon misses the deadline.
    (cd "$work/$arm-$rep" && "$bin" prepare-repo . >/dev/null 2>&1 &&
      "$bin" scope-task "warm up the index" --metrics off >/dev/null 2>&1) || true
  done
done

{
  echo "ref=$ref ($(git -C "$source_repo" rev-parse "$ref^{commit}"))"
  echo "model=$model effort=$effort"
  echo "claude=$(claude --version)"
  echo "pixel=$("$pixel_bin" --version | head -2 | tr '\n' ' ')"
  echo "prompt_sha256=$(shasum -a 256 "$prompt_file" | cut -d' ' -f1)"
  if [ -n "$cand_bin" ]; then
    echo "candidate_bin=$cand_bin ($("$cand_bin" --version | head -2 | tr '\n' ' '))"
    [ -z "$candidate" ] || echo "warning=CANDIDATE_PROMPT and CANDIDATE_BIN both set: two variables change"
  fi
  if [ -n "$candidate" ]; then
    echo "candidate_prompt=$candidate"
    echo "candidate_prompt_sha256=$(shasum -a 256 "$candidate" | cut -d' ' -f1)"
  fi
  echo "isolation=fresh repository with REF's history only, one copy and daemon per run, per-run PIXEL_SESSION_ID"
  echo "task=$task"
  echo "arms=${ARMS:-vanilla pixel} reps=$reps started=$(date -u +%FT%TZ)"
} > "$out/meta.txt"

stamp() { python3 -u -c '
import sys, json, time
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        e = json.loads(line)
    except ValueError:
        continue
    print(json.dumps({"t": int(time.time() * 1000), "e": e}), flush=True)
'; }

run() {
  local arm=$1 rep=$2
  local extra=() path=$PATH
  case $arm in
    pixel) extra=(--settings "$settings") ;;
    candidate)
      [ -n "$candidate$cand_bin" ] || { echo "arm candidate needs CANDIDATE_PROMPT or CANDIDATE_BIN" >&2; return 1; }
      extra=(--settings "$candidate_settings"); path=$cand_path ;;
  esac
  (cd "$work/$arm-$rep" && env -u CLAUDECODE PATH="$path" PIXEL_SESSION_ID="demo-$arm-$rep" claude -p "$task" \
    --model "$model" --effort "$effort" \
    --setting-sources "" --disable-slash-commands --strict-mcp-config \
    --append-system-prompt "$english" \
    --allowedTools "Bash Read Grep Glob" \
    --disallowedTools "Edit Write NotebookEdit Agent Task WebFetch WebSearch" \
    --output-format stream-json --verbose \
    ${extra[@]+"${extra[@]}"} < /dev/null) | stamp > "$out/$arm-$rep.jsonl"
}

par=${PAR:-3}
for rep in $(seq 1 "$reps"); do
  for arm in ${ARMS:-vanilla pixel}; do run "$arm" "$rep" & done
  if [ $((rep % par)) -eq 0 ]; then wait; fi
done
wait

# Whether each Pixel run's prompt-submit hook delivered its task packet:
# stream-json shows no UserPromptSubmit event, but the hook records the
# session in the copy's task runtime before it prints the packet.
for f in "$out"/*.jsonl; do
  run=$(basename "$f" .jsonl)
  case $run in vanilla-*) continue ;; esac
  python3 - "$f" "$work/$run/.pixel/task-runtime.json" <<'PY'
import json, os, sys
f, runtime = sys.argv[1:]
sid = next((json.loads(l)["e"].get("session_id") for l in open(f) if '"init"' in l), None)
try:
    sessions = {s.get("session_id") for s in json.load(open(runtime)).get("sessions", [])}
except (OSError, ValueError):
    sessions = set()
print(f"{os.path.basename(f)[:-6]} packet={'yes' if sid in sessions else 'no'}")
PY
done | sort -V > "$out/packets.txt"
