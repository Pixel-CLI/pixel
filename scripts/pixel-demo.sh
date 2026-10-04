#!/usr/bin/env bash
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

# pixel-demo.sh — side-by-side demo: agent task with vs without Pixel.
#
# Runs ONE scenario through `claude -p` twice:
#   1. BASELINE (no Pixel hooks, --safe-mode, pixel off PATH)
#   2. WITH PIXEL (full hooks, pixel on PATH)
#
# Prints a clear comparison: wall-clock time, tool calls, turns, and
# whether pixel was actually used. Full transcripts saved for review.
#
# Usage:
#   scripts/pixel-demo.sh                    # default scenario: scope
#   SCENARIO=locate scripts/pixel-demo.sh    # choose scenario
#   scripts/pixel-demo.sh /path/to/repo      # custom repo
#
# Scenarios:
#   locate  — find a symbol definition + context
#   scope   — find all files affected by a change (impact analysis)
#   sync    — sync branch with origin/main
#   recover — find a deleted function in git history
#
# Prerequisites:
#   - claude CLI installed and authenticated
#   - pixel installed (`pixel install` run once: the pixel arm is given the
#     deployed agent prompt the way the `claude` shell wrapper does); PIXEL_BIN
#     and AGENT_PROMPT override
#   - repo indexed (done here on first run)

set -euo pipefail

REPO="${1:-$(cd "$(dirname "$0")/.." && pwd)}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
# Binary: $PIXEL_BIN, else the installed pixel (`command -v pixel`: mise shim,
# Homebrew, ~/.cargo/bin, ~/.local/bin), else a local build (dev-release is the
# reinstall-loop profile, see CONTRIBUTING.md; release is the shipped one).
PIXEL_BIN="${PIXEL_BIN:-$(command -v pixel 2>/dev/null || true)}"
if [ -z "$PIXEL_BIN" ]; then
  for p in "$ROOT/target/dev-release/pixel" "$ROOT/target/release/pixel"; do
    [ -x "$p" ] && PIXEL_BIN="$p" && break
  done
fi
SCENARIO="${SCENARIO:-scope}"
OUTDIR="/tmp/pixel-demo"
mkdir -p "$OUTDIR"

cd "$REPO"

# ── Colors ──────────────────────────────────────────────────────────────
B='\033[1m'; R='\033[0m'; RED='\033[31m'; GRN='\033[32m'; YLW='\033[33m'
BLU='\033[34m'; CYA='\033[36m'; DIM='\033[2m'

# ── Prerequisites ───────────────────────────────────────────────────────
if ! command -v claude &>/dev/null; then
  echo -e "${RED}ERROR:${R} claude CLI not found. Install Claude Code first." >&2
  exit 1
fi
if [ -z "$PIXEL_BIN" ] || [ ! -x "$PIXEL_BIN" ]; then
  echo -e "${RED}ERROR:${R} pixel binary not found (set PIXEL_BIN, install pixel, or build it)" >&2
  exit 1
fi
# The pixel arm must receive what the `claude` shell wrapper written by
# `pixel install` injects (this script runs `claude` by path, so a fish/zsh
# function never applies): the deployed agent prompt, or the bundled asset
# when nothing is installed.
AGENT_PROMPT="${AGENT_PROMPT:-$HOME/.local/share/pixel/agent-prompt.md}"
[ -s "$AGENT_PROMPT" ] || AGENT_PROMPT="$ROOT/crates/pixel-install/assets/pixel-agent-prompt.md"
SUBAGENT_PROMPT="${SUBAGENT_PROMPT:-$HOME/.local/share/pixel/subagent-prompt.md}"
[ -s "$SUBAGENT_PROMPT" ] || SUBAGENT_PROMPT="$ROOT/crates/pixel-install/assets/pixel-subagent-prompt.md"
if [ ! -s "$AGENT_PROMPT" ]; then
  echo "ERROR: agent prompt not found (run \`pixel install\`, or set AGENT_PROMPT)" >&2
  exit 1
fi

# ── Build pixel-free baseline settings ──────────────────────────────────
CLAUDE_SETTINGS="${CLAUDE_SETTINGS:-$HOME/.claude/settings.json}"
BASELINE_SETTINGS="$OUTDIR/baseline-settings.json"
python3 - "$CLAUDE_SETTINGS" "$BASELINE_SETTINGS" << 'PY'
import json, sys
src, dst = sys.argv[1], sys.argv[2]
try:
    with open(src) as f:
        cfg = json.load(f)
except (OSError, json.JSONDecodeError):
    cfg = {}
hooks = cfg.get("hooks", {})
if isinstance(hooks, dict):
    for event in list(hooks.keys()):
        entries = hooks[event]
        if isinstance(entries, list):
            hooks[event] = [e for e in entries if not (isinstance(e, dict) and "pixel" in str(e.get("command", "")))]
        elif isinstance(entries, dict):
            for k in list(entries.keys()):
                if "pixel" in str(k):
                    del entries[k]
                elif isinstance(entries[k], list):
                    entries[k] = [e for e in entries[k] if not (isinstance(e, dict) and "pixel" in str(e.get("command", "")))]
cfg["hooks"] = hooks
with open(dst, "w") as f:
    json.dump(cfg, f, indent=2)
PY

PIXEL_DIR="$(dirname "$PIXEL_BIN")"
BASELINE_PATH="$PATH"
PIXEL_PATH="$PIXEL_DIR:$BASELINE_PATH"

# ── Scenario prompts (natural language, no tool instructions) ───────────
write_prompt() {
  local scenario="$1" file="$2"
  case "$scenario" in
    locate)
      cat > "$file" << 'PROMPT'
You are working in the repository REPO_PLACEHOLDER (a Rust CLI tool).

Find where GUARD_MATCHER is defined and show its full definition with surrounding context. Report the file path, line number, and the full definition.
PROMPT
      ;;
    scope)
      cat > "$file" << 'PROMPT'
You are working in the repository REPO_PLACEHOLDER (a Rust CLI tool).

I want to add a new agent tool called "foobar" to the guard matcher. Find ALL files that would need to be modified for this change. List every file and why it needs changes.
PROMPT
      ;;
    sync)
      cat > "$file" << 'PROMPT'
You are working in the repository REPO_PLACEHOLDER (a Rust CLI tool).

Sync this branch with origin/main. Report what happened.
PROMPT
      ;;
    recover)
      cat > "$file" << 'PROMPT'
You are working in the repository REPO_PLACEHOLDER (a Rust CLI tool).

Find the deleted function register_mcp_server that was removed from the codebase. Show the commit that removed it, the file it was in, and the full original implementation.
PROMPT
      ;;
    *)
      echo "Unknown scenario: $scenario" >&2; exit 1
      ;;
  esac
  if [[ "$OSTYPE" == "darwin"* ]]; then
    sed -i '' "s|REPO_PLACEHOLDER|$REPO|g" "$file"
  else
    sed -i "s|REPO_PLACEHOLDER|$REPO|g" "$file"
  fi
}

PROMPT_FILE="$OUTDIR/prompt.txt"
write_prompt "$SCENARIO" "$PROMPT_FILE"

# ── Run one arm ─────────────────────────────────────────────────────────
# run_arm <arm> <settings> <PATH> [extra claude flags...]
run_arm() {
  local arm="$1" settings="$2" path_env="$3"
  shift 3
  local outfile="$OUTDIR/${arm}.json"
  local start end ms

  echo -e "  ${DIM}Running ${arm}...${R}" >&2
  start=$(python3 -c 'import time; print(int(time.time()*1000))')
  PATH="$path_env" claude -p --dangerously-skip-permissions --verbose \
    "$@" \
    --settings "$settings" --output-format stream-json \
    < "$PROMPT_FILE" > "$outfile" 2>&1 || true
  end=$(python3 -c 'import time; print(int(time.time()*1000))')
  ms=$((end - start))

  # Parse tool calls, turns, validity
  python3 - "$outfile" "$ms" << 'PY'
import json, sys, re

path = sys.argv[1]
wall_ms = int(sys.argv[2])
tool_calls = 0
turns = 0
assistant_events = 0
valid = 0
api_ms = 0
pixel_calls = 0
tools_used = {}

pixel_pat = re.compile(r"(^|[\s/;&|])pixel\s+(search|resolve|targets|reconcile|excavate|rescue|impact|uses|changes|context|symbol|inspect|history|publish|push|ship|branch|update|sync|diff|review|ask|recall)")

try:
    with open(path) as f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            try:
                evt = json.loads(line)
            except json.JSONDecodeError:
                continue
            if not isinstance(evt, dict):
                continue
            if evt.get("type") == "assistant":
                turns += 1
                assistant_events += 1
                msg = evt.get("message")
                content = msg.get("content") if isinstance(msg, dict) else None
                if content is None:
                    content = evt.get("content")
                if isinstance(content, list):
                    for block in content:
                        if isinstance(block, dict) and block.get("type") == "tool_use":
                            tool_calls += 1
                            tool_name = block.get("name", "unknown")
                            tools_used[tool_name] = tools_used.get(tool_name, 0) + 1
                            inp = json.dumps(block.get("input", {}))
                            if pixel_pat.search(inp):
                                pixel_calls += 1
            elif evt.get("type") == "result":
                res_text = str(evt.get("result", ""))
                if evt.get("subtype") == "success" and "Not logged in" not in res_text:
                    valid = 1
                if isinstance(evt.get("duration_ms"), int):
                    api_ms = evt["duration_ms"]
except Exception:
    pass

if assistant_events == 0:
    valid = 0

# Output as TSV for easy parsing
print(f"{wall_ms}\t{tool_calls}\t{turns}\t{valid}\t{api_ms}\t{pixel_calls}")
# Tools breakdown to stderr
tools_str = ", ".join(f"{k}×{v}" for k, v in sorted(tools_used.items(), key=lambda x: -x[1]))
print(f"TOOLS:{tools_str}", file=sys.stderr)
PY
}

# ── Header ──────────────────────────────────────────────────────────────
echo ""
echo -e "${B}${CYA}╔══════════════════════════════════════════════════════════════╗${R}"
echo -e "${B}${CYA}║          PIXEL A/B DEMO — with vs without Pixel             ║${R}"
echo -e "${B}${CYA}╚══════════════════════════════════════════════════════════════╝${R}"
echo ""
echo -e "  ${B}Scenario:${R}  $SCENARIO"
echo -e "  ${B}Repo:${R}     $REPO"
echo -e "  ${B}Pixel:${R}    $($PIXEL_BIN --version 2>/dev/null || echo 'not found')"
echo -e "  ${B}Claude:${R}   $(claude --version 2>/dev/null || echo 'not found')"
echo -e "  ${B}Prompt:${R}   $(cat "$PROMPT_FILE" | head -3 | tr '\n' ' ')"
echo ""
echo -e "  ${DIM}Full transcripts saved to $OUTDIR/{baseline,pixel}.json${R}"
echo ""

# ── Pre-index for pixel arm ─────────────────────────────────────────────
echo -e "  ${DIM}Pre-indexing repo for pixel arm...${R}"
PATH="$PIXEL_PATH" "$PIXEL_BIN" build-index "$REPO" 2>/dev/null || true
echo ""

# ── Run both arms ───────────────────────────────────────────────────────
echo -e "${B}${YLW}━━━ ARM 1: BASELINE (no Pixel) ━━━${R}"
BASELINE_RESULT=$(run_arm "baseline" "$BASELINE_SETTINGS" "$BASELINE_PATH" "--safe-mode" 2>"$OUTDIR/baseline-tools.txt")
BASELINE_TOOLS=$(cat "$OUTDIR/baseline-tools.txt")
echo ""

echo -e "${B}${GRN}━━━ ARM 2: WITH PIXEL ━━━${R}"
PIXEL_RESULT=$(run_arm "pixel" "$CLAUDE_SETTINGS" "$PIXEL_PATH" \
  --append-system-prompt-file "$AGENT_PROMPT" \
  --append-subagent-system-prompt-file "$SUBAGENT_PROMPT" 2>"$OUTDIR/pixel-tools.txt")
PIXEL_TOOLS=$(cat "$OUTDIR/pixel-tools.txt")
echo ""

# ── Parse results ───────────────────────────────────────────────────────
read -r B_MS B_TOOLS B_TURNS B_VALID B_API B_PIXEL <<< "$BASELINE_RESULT"
read -r P_MS P_TOOLS P_TURNS P_VALID P_API P_PIXEL <<< "$PIXEL_RESULT"

# ── Comparison table ────────────────────────────────────────────────────
echo -e "${B}${CYA}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${R}"
echo -e "${B}${CYA}                    RESULTS COMPARISON                         ${R}"
echo -e "${B}${CYA}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${R}"
echo ""
printf "  %-20s ${RED}%-15s${R}  ${GRN}%-15s${R}\n" "Metric" "BASELINE" "WITH PIXEL"
printf "  %-20s %-15s  %-15s\n" "────────────────────" "───────────────" "───────────────"

# Time
if [ "${B_VALID:-0}" = "1" ] && [ "${P_VALID:-0}" = "1" ]; then
  DELTA_MS=$((P_MS - B_MS))
  DELTA_PCT=$((DELTA_MS * 100 / B_MS))
  if [ $DELTA_MS -lt 0 ]; then
    TIME_SIGN="${GRN}▲${R}"
    TIME_STR="${GRN}${P_MS}ms (${DELTA_PCT}%)${R}"
  else
    TIME_SIGN="${RED}▼${R}"
    TIME_STR="${RED}${P_MS}ms (+${DELTA_PCT}%)${R}"
  fi
  printf "  %-20s %-15s  " "Wall-clock time" "${B_MS}ms"
  echo -e "$TIME_STR"
  printf "  %-20s %-15s  %-15s\n" "API time" "${B_API}ms" "${P_API}ms"
  printf "  %-20s %-15s  %-15s\n" "Tool calls" "$B_TOOLS" "$P_TOOLS"
  printf "  %-20s %-15s  %-15s\n" "Agent turns" "$B_TURNS" "$P_TURNS"
  printf "  %-20s %-15s  %-15s\n" "Pixel invocations" "$B_PIXEL" "$P_PIXEL"
else
  printf "  ${RED}%-20s%n${R}" "INVALID RUN — check transcripts"
fi

echo ""
echo -e "${B}Tools used (baseline):${R}  ${DIM}${BASELINE_TOOLS:-none}${R}"
echo -e "${B}Tools used (pixel):${R}     ${DIM}${PIXEL_TOOLS:-none}${R}"
echo ""

# ── Verdict ─────────────────────────────────────────────────────────────
if [ "${B_VALID:-0}" = "1" ] && [ "${P_VALID:-0}" = "1" ]; then
  echo -e "${B}${CYA}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${R}"
  if [ $DELTA_MS -lt 0 ]; then
    echo -e "  ${GRN}${B}Pixel was $((DELTA_MS * -1))ms faster ($((DELTA_PCT * -1))% reduction)${R}"
  else
    echo -e "  ${RED}${B}Pixel was ${DELTA_MS}ms slower (+${DELTA_PCT}%)${R}"
  fi
  if [ "$P_PIXEL" -gt 0 ] && [ "$B_PIXEL" -eq 0 ]; then
    echo -e "  ${GRN}✓${R} Pixel arm used pixel commands; baseline stayed pixel-free"
  elif [ "$B_PIXEL" -gt 0 ]; then
    echo -e "  ${YLW}⚠${R} Baseline was contaminated (used pixel $B_PIXEL times)"
  fi
  if [ "$P_TOOLS" -lt "$B_TOOLS" ]; then
    echo -e "  ${GRN}✓${R} Pixel used $((B_TOOLS - P_TOOLS)) fewer tool calls"
  elif [ "$P_TOOLS" -gt "$B_TOOLS" ]; then
    echo -e "  ${YLW}⚠${R} Pixel used $((P_TOOLS - B_TOOLS)) more tool calls"
  fi
  echo -e "${B}${CYA}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${R}"
fi
echo ""
echo -e "  ${DIM}Transcripts: $OUTDIR/baseline.json  $OUTDIR/pixel.json${R}"
echo ""
