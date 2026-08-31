#!/usr/bin/env bash
# pixel-bench.sh — measure agent workflow time with and without pixel.
#
# Runs 4 scenarios via `claude -p` (default model, --dangerously-skip-permissions):
#   1. Locating code by phrase
#   2. Scoping a task before editing
#   3. Syncing a branch
#   4. Recovering deleted code
#
# Each scenario runs N times (default 3) per arm. Arms (baseline vs pixel) are
# order-randomized per scenario so neither arm benefits from warm caches.
# Scenarios run SERIALLY (not in parallel) so resource contention can't skew
# wall-clock. The baseline is truly pixel-free: it runs with --safe-mode
# (skips CLAUDE.md memory, hooks, and skills while keeping OAuth) PLUS the
# pixel hooks stripped from a copy of settings.json PLUS pixel off PATH.
# Settings-stripping alone is NOT enough: the installed CLAUDE.md rule text
# mandates pixel by absolute path, and a 2026-08-30 run measured 12/12
# baseline cells self-contaminating without --safe-mode.
#
# Results record wall-clock ms, tool-call count, and turn count per run, plus
# a pixel-usage check (did the pixel arm actually invoke pixel?).
#
# Usage:
#   scripts/pixel-bench.sh                    # uses ~/pixel as repo
#   scripts/pixel-bench.sh /path/to/repo      # custom repo
#   PIXEL_BIN=/custom/pixel scripts/pixel-bench.sh
#   N=5 scripts/pixel-bench.sh                # 5 reps per cell
#
# Prerequisites:
#   - claude (Claude Code CLI) installed and authenticated
#   - pixel built (cargo build --release -p pixel-cli)
#   - The target repo indexed (pixel index .) and pixel rules in ~/.claude/CLAUDE.md

set -euo pipefail

REPO="${1:-$(cd "$(dirname "$0")/.." && pwd)}"
PIXEL_BIN="${PIXEL_BIN:-$(cd "$(dirname "$0")/.." && pwd)/target/release/pixel}"
N="${N:-3}"
OUTDIR="/tmp/pixel-bench-outputs"
TMPDIR_M="/tmp/pixel-bench-tmp"
RESULTS="${RESULTS:-$(pwd)/docs/bench/pixel-bench-results.txt}"
mkdir -p "$OUTDIR" "$TMPDIR_M"
rm -f "$TMPDIR_M"/*.json "$TMPDIR_M"/*.ms "$TMPDIR_M"/*.counts

cd "$REPO"

# Verify prerequisites
if ! command -v claude &>/dev/null; then
  echo "ERROR: claude CLI not found. Install Claude Code first." >&2
  exit 1
fi
if [ ! -x "$PIXEL_BIN" ]; then
  echo "ERROR: pixel binary not found at $PIXEL_BIN. Run: cargo build --release -p pixel-cli" >&2
  exit 1
fi

# --- Build a truly pixel-free baseline settings.json ---
# Copy ~/.claude/settings.json and strip every hook whose command references
# pixel (PreToolUse guard, SessionStart). The baseline then never invokes
# pixel — no PATH shim, no absolute-path guard shim.
CLAUDE_SETTINGS="${CLAUDE_SETTINGS:-$HOME/.claude/settings.json}"
BASELINE_SETTINGS="/tmp/pixel-bench-baseline-settings.json"
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
            kept = []
            for e in entries:
                cmd = str(e.get("command", "")) if isinstance(e, dict) else ""
                if "pixel" in cmd:
                    continue
                kept.append(e)
            hooks[event] = kept
        elif isinstance(entries, dict):
            # nested { "hooks": [ {command:...} ] } form
            for k in list(entries.keys()):
                if "pixel" in str(k):
                    del entries[k]
                elif isinstance(entries[k], list):
                    entries[k] = [e for e in entries[k]
                                  if not (isinstance(e, dict) and "pixel" in str(e.get("command", "")))]
cfg["hooks"] = hooks
with open(dst, "w") as f:
    json.dump(cfg, f, indent=2)
PY

# --- Baseline isolation caveat (all alternatives tested 2026-08-30, CLI
# v2.1.251): a truly pixel-free baseline is IMPOSSIBLE with keychain OAuth.
#   - CLAUDE_CONFIG_DIR (even =~/.claude) -> file-based creds, auth fails
#   - HOME=/tmp/... -> auth fails (exit 1)
#   - --bare -> requires ANTHROPIC_API_KEY (banned on this machine)
#   - --system-prompt -> OAuth works but global CLAUDE.md memory still loads
#     (probe: model still sees the pixel rules)
# Auth failures are reported as subtype "success" with result "Not logged in"
# and NO assistant events, but a NON-ZERO process exit code. The baseline
# therefore shares the user's real config (global CLAUDE.md rules included,
# which mandate pixel). True pixel-freeness cannot be guaranteed; the usage
# check below DETECTS contamination per run and flags it, so a contaminated
# baseline is visible rather than silently wrong.

# PATHs: keep the full user PATH in BOTH arms — stripping /opt/homebrew/bin
# breaks node-based hooks (`node: command not found`) and adds failure noise
# that corrupts the measurement. Baseline pixel-freeness comes from the
# stripped settings.json (no pixel hooks), and is VERIFIED per-run by the
# usage check below (which scans baseline transcripts for pixel invocations
# and flags contamination — e.g. the agent calling pixel voluntarily because
# global CLAUDE.md rules mandate it).
PIXEL_DIR="$(dirname "$PIXEL_BIN")"
BASELINE_PATH="$PATH"
PIXEL_PATH="$PIXEL_DIR:$BASELINE_PATH"

# Prompt files — natural language, no tool instructions
write_prompts() {
  local dir="$1"
  mkdir -p "$dir"
  cat > "$dir/s1-locate.txt" << 'PROMPT'
You are working in the repository REPO_PLACEHOLDER (a Rust CLI tool).

Find where GUARD_MATCHER is defined and show its full definition with surrounding context. Report the file path, line number, and the full definition.
PROMPT
  cat > "$dir/s2-scope.txt" << 'PROMPT'
You are working in the repository REPO_PLACEHOLDER (a Rust CLI tool).

I want to add a new agent tool called "foobar" to the guard matcher. Find ALL files that would need to be modified for this change. List every file and why it needs changes.
PROMPT
  cat > "$dir/s3-sync.txt" << 'PROMPT'
You are working in the repository REPO_PLACEHOLDER (a Rust CLI tool).

Sync this branch with origin/main. Report what happened.
PROMPT
  cat > "$dir/s4-recover.txt" << 'PROMPT'
You are working in the repository REPO_PLACEHOLDER (a Rust CLI tool).

Find the deleted function register_mcp_server that was removed from the codebase. Show the commit that removed it, the file it was in, and the full original implementation.
PROMPT
  # Replace placeholder with actual repo path
  if [[ "$OSTYPE" == "darwin"* ]]; then
    sed -i '' "s|REPO_PLACEHOLDER|$REPO|g" "$dir"/*.txt
  else
    sed -i "s|REPO_PLACEHOLDER|$REPO|g" "$dir"/*.txt
  fi
}

PROMPT_DIR="/tmp/pixel-bench-prompts"
write_prompts "$PROMPT_DIR"

# --- Preflight: one cheap end-to-end claude run must succeed before the
# matrix starts. Catches instantly-dying runs (auth, env, killed children)
# BEFORE burning 4 scenarios x 2 arms x N reps on corpses.
echo "preflight: verifying claude -p produces a successful result event..."
PREFLIGHT_OUT="$TMPDIR_M/preflight.json"
printf 'Reply with the single word OK and do nothing else.' | \
  PATH="$BASELINE_PATH" claude -p --dangerously-skip-permissions --verbose \
    --settings "$BASELINE_SETTINGS" --output-format stream-json \
    > "$PREFLIGHT_OUT" 2>&1 || true
# NOTE: the CLI reports auth failures as subtype "success" with the result
# text "Not logged in" and zero streamed assistant events — check all three.
if ! grep -q '"type":"result"' "$PREFLIGHT_OUT" \
   || ! grep -q '"subtype":"success"' "$PREFLIGHT_OUT" \
   || grep -q 'Not logged in' "$PREFLIGHT_OUT" \
   || ! grep -q '"type":"assistant"' "$PREFLIGHT_OUT"; then
  echo "ERROR: preflight claude run failed (auth failure or dead env)." >&2
  echo "       The benchmark would only measure dead runs. Last output lines:" >&2
  tail -5 "$PREFLIGHT_OUT" >&2
  exit 2
fi
echo "preflight: OK"
echo "NOTE: full matrix = 4 scenarios x 2 arms x $N reps; expect 10-40+ minutes." \
     "Do not run under a short shell timeout."

# Run one cell: wall-clock ms + tool-call/turn counts from stream-json output.
# Optional 5th arg: extra claude flags (e.g. --safe-mode for the baseline arm).
run_scenario() {
  local label="$1"
  local prompt_file="$2"
  local settings="$3"
  local path_env="$4"
  local extra_flag="${5:-}"
  local start end ms
  start=$(python3 -c 'import time; print(int(time.time()*1000))')
  PATH="$path_env" claude -p --dangerously-skip-permissions --verbose \
    $extra_flag \
    --settings "$settings" --output-format stream-json \
    < "$prompt_file" > "$OUTDIR/${label}.json" 2>&1 || true
  end=$(python3 -c 'import time; print(int(time.time()*1000))')
  ms=$((end - start))
  echo "${ms}" > "$TMPDIR_M/${label}.ms"
  python3 - "$OUTDIR/${label}.json" "$TMPDIR_M/${label}.counts" << 'PY'
import json, sys
path, out = sys.argv[1], sys.argv[2]
tool_calls = 0
turns = 0
assistant_events = 0  # real model turns observed in the stream
valid = 0            # 1 only if the stream contains a successful result event
api_ms = 0           # API-reported duration from the result event
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
                # stream-json nests blocks under message.content; accept the
                # bare content form too for robustness.
                msg = evt.get("message")
                content = msg.get("content") if isinstance(msg, dict) else None
                if content is None:
                    content = evt.get("content")
                if isinstance(content, list):
                    for block in content:
                        if isinstance(block, dict) and block.get("type") == "tool_use":
                            tool_calls += 1
            elif evt.get("type") == "result":
                # CAUTION: the CLI reports auth failures ("Not logged in")
                # as subtype "success" — a result event alone proves nothing.
                res_text = str(evt.get("result", ""))
                if evt.get("subtype") == "success" and "Not logged in" not in res_text:
                    valid = 1
                if turns == 0 and isinstance(evt.get("num_turns"), int):
                    turns = evt["num_turns"]
                if isinstance(evt.get("duration_ms"), int):
                    api_ms = evt["duration_ms"]
except Exception:
    pass
# A run with zero assistant events never reached the model — invalid even if
# a "success" result event is present (num_turns in the result event can be
# nonzero on auth-failure results, so count only real streamed events).
if assistant_events == 0:
    valid = 0
with open(out, "w") as f:
    f.write(f"{tool_calls} {turns} {valid} {api_ms}\n")
PY
  # Run-validity gate: a run with no successful result event is a dead run
  # (killed process, auth failure, crash). Mark it loudly — dead runs must
  # never silently enter the means.
  read -r _tc _tn _valid _ams < "$TMPDIR_M/${label}.counts" || _valid=0
  if [ "${_valid:-0}" != "1" ]; then
    echo "  WARNING: $label produced NO successful result event — run is INVALID (excluded from means)" >&2
  fi
}

# Override for smoke tests, e.g.: SCENARIOS="s1-locate" N=1 scripts/pixel-bench.sh
SCENARIOS="${SCENARIOS:-s1-locate s2-scope s3-sync s4-recover}"

echo "=== pixel-bench: claude -p agent workflows ===" > "$RESULTS"
echo "Date: $(date -u +%Y-%m-%dT%H:%M:%SZ)" >> "$RESULTS"
echo "Repo: $(git rev-parse --short HEAD) ($REPO)" >> "$RESULTS"
echo "Pixel: $($PIXEL_BIN --version 2>/dev/null || echo 'not built')" >> "$RESULTS"
echo "Reps per cell: $N" >> "$RESULTS"
echo "Baseline settings (pixel hooks stripped): $BASELINE_SETTINGS" >> "$RESULTS"
echo "" >> "$RESULTS"

# Index the repo once before any pixel arm (lazy index would otherwise skew
# the first pixel run).
PATH="$PIXEL_PATH" "$PIXEL_BIN" index "$REPO" 2>/dev/null || true

# --- Serial scenarios, order-randomized arms ---
for s in $SCENARIOS; do
  echo "--- scenario $s ---" >> "$RESULTS"
  # Randomize arm order so neither arm always benefits from warm caches.
  if [ $((RANDOM % 2)) -eq 0 ]; then
    ARM_ORDER="baseline pixel"
  else
    ARM_ORDER="pixel baseline"
  fi
  for arm in $ARM_ORDER; do
    for i in $(seq 1 "$N"); do
      label="${arm}-${s}-${i}"
      if [ "$arm" = "baseline" ]; then
        # --safe-mode: skip CLAUDE.md memory, hooks, and skills while keeping
        # OAuth. Without it the installed rule text mandates pixel by absolute
        # path and the baseline arm self-contaminates (measured 2026-08-30:
        # 12/12 baseline runs invoked pixel — see
        # docs/bench/agent-ab-2026-08-30-rerun-contaminated.txt).
        run_scenario "$label" "$PROMPT_DIR/$s.txt" "$BASELINE_SETTINGS" "$BASELINE_PATH" --safe-mode
      else
        run_scenario "$label" "$PROMPT_DIR/$s.txt" "$CLAUDE_SETTINGS" "$PIXEL_PATH"
      fi
      ms=$(cat "$TMPDIR_M/$label.ms")
      read -r tool_calls turns valid api_ms < "$TMPDIR_M/$label.counts"
      if [ "${valid:-0}" = "1" ]; then
        echo "  $label: ${ms}ms (api=${api_ms}ms) tools=${tool_calls} turns=${turns}" >> "$RESULTS"
      else
        echo "  $label: INVALID (no successful result event; wall=${ms}ms) — excluded" >> "$RESULTS"
      fi
    done
  done
done

# --- Pixel-usage check on BOTH arms, recorded into the results file ---
# pixel arm: did the agent actually use pixel? baseline arm: contamination
# check — the baseline must NOT have used pixel.
# The check parses tool_use INPUTS from the stream-json transcript. A plain
# grep over the whole transcript false-positives on file CONTENT the agent
# read (this repo's docs are full of pixel command examples) — measured
# 2026-08-30: string-grep flagged 12/12 clean baseline cells as contaminated
# while the tool_use-level parse showed 0 real invocations.
echo "" >> "$RESULTS"
echo "=== PIXEL USAGE CHECK (tool_use-level) ===" >> "$RESULTS"
for arm in pixel baseline; do
  for s in $SCENARIOS; do
    for i in $(seq 1 "$N"); do
      label="${arm}-${s}-${i}"
      used=$(python3 - "$OUTDIR/$label.json" << 'PY'
import json, re, sys
pat = re.compile(r"(^|[\s/;&|])pixel\s+(search|resolve|targets|reconcile|excavate|rescue|impact|uses|changes|context|symbol|inspect|history|history-search|lifecycle|publish|push|ship|branch|update|sync|diff|review)")
count = 0
try:
    with open(sys.argv[1]) as f:
        for line in f:
            try:
                ev = json.loads(line)
            except Exception:
                continue
            msg = ev.get("message") or {}
            content = msg.get("content")
            if not isinstance(content, list):
                continue
            for blk in content:
                if isinstance(blk, dict) and blk.get("type") == "tool_use":
                    if pat.search(json.dumps(blk.get("input", {}))):
                        count += 1
except FileNotFoundError:
    pass
print(count)
PY
)
      used=${used:-0}
      if [ "$arm" = "pixel" ]; then
        if [ "$used" -gt 0 ] 2>/dev/null; then
          echo "  $label: pixel used ($used invocations)" >> "$RESULTS"
        else
          echo "  $label: pixel NOT used (fell back to grep/git)" >> "$RESULTS"
        fi
      else
        if [ "$used" -gt 0 ] 2>/dev/null; then
          echo "  $label: CONTAMINATED — baseline invoked pixel $used time(s) (arm not pixel-free)" >> "$RESULTS"
        else
          echo "  $label: baseline pixel-free" >> "$RESULTS"
        fi
      fi
    done
  done
done

echo "=== done ===" >> "$RESULTS"

# Print summary table (mean over N runs)
echo ""
echo "=== RESULTS ==="
cat "$RESULTS"

echo ""
echo "=== COMPARISON (mean over valid runs only; INVALID runs excluded) ==="
python3 - "$N" "$TMPDIR_M" $SCENARIOS << 'PY'
import sys, os, statistics

n, d = int(sys.argv[1]), sys.argv[2]
scenarios = sys.argv[3:]

def cell(arm, s):
    """Return (mean_ms, mean_tools, mean_turns, valid_count) over VALID runs."""
    ms, tools, turns = [], [], []
    for i in range(1, n + 1):
        cp = os.path.join(d, f"{arm}-{s}-{i}.counts")
        mp = os.path.join(d, f"{arm}-{s}-{i}.ms")
        if not (os.path.exists(cp) and os.path.exists(mp)):
            continue
        try:
            parts = open(cp).read().split()
            tc, tn = int(parts[0]), int(parts[1])
            valid = int(parts[2]) if len(parts) > 2 else 0
            wall = int(open(mp).read().strip())
        except Exception:
            continue
        if valid != 1:
            continue
        ms.append(wall); tools.append(tc); turns.append(tn)
    if not ms:
        return None
    return (int(statistics.mean(ms)), int(statistics.mean(tools)),
            int(statistics.mean(turns)), len(ms))

hdr = f"{'Scenario':<25} {'Baseline':>12} {'WithPixel':>12} {'Delta':>8} {'Tools b/p':>10} {'Turns b/p':>10} {'Valid':>7}"
print(hdr); print("-" * len(hdr))
broken = False
for s in scenarios:
    b, p = cell("baseline", s), cell("pixel", s)
    if b is None or p is None:
        which = [name for name, c in (("baseline", b), ("pixel", p)) if c is None]
        print(f"{s:<25} *** NO VALID RUNS in {'/'.join(which)} arm — comparison meaningless, fix the harness/run first ***")
        broken = True
        continue
    delta = (p[0] - b[0]) * 100 // b[0] if b[0] else 0
    print(f"{s:<25} {b[0]:>10}ms {p[0]:>10}ms {delta:>7}% {str(b[1])+'/'+str(p[1]):>10} {str(b[2])+'/'+str(p[2]):>10} {str(b[3])+'+'+str(p[3]):>7}")
if broken:
    sys.exit(3)
PY
