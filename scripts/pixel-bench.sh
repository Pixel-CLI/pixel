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
# wall-clock. The baseline is truly pixel-free: the pixel hooks are stripped
# from a copy of settings.json, so pixel is never invoked — no PATH shim.
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
RESULTS="${RESULTS:-$(pwd)/pixel-bench-results.txt}"
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

# PATHs: baseline excludes pixel entirely; pixel arm puts pixel dir first so
# the guard hook's `exec pixel hook guard` resolves.
PIXEL_DIR="$(dirname "$PIXEL_BIN")"
BASELINE_PATH="/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin:/usr/games:/usr/local/games:/snap/bin:$HOME/.cargo/bin"
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

# Run one cell: wall-clock ms + tool-call/turn counts from stream-json output.
run_scenario() {
  local label="$1"
  local prompt_file="$2"
  local settings="$3"
  local path_env="$4"
  local start end ms
  start=$(python3 -c 'import time; print(int(time.time()*1000))')
  PATH="$path_env" claude -p --dangerously-skip-permissions \
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
                content = evt.get("content")
                if isinstance(content, list):
                    for block in content:
                        if isinstance(block, dict) and block.get("type") == "tool_use":
                            tool_calls += 1
            elif evt.get("type") == "result" and turns == 0:
                nt = evt.get("num_turns")
                if isinstance(nt, int):
                    turns = nt
except Exception:
    pass
with open(out, "w") as f:
    f.write(f"{tool_calls} {turns}\n")
PY
}

SCENARIOS="s1-locate s2-scope s3-sync s4-recover"

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
        run_scenario "$label" "$PROMPT_DIR/$s.txt" "$BASELINE_SETTINGS" "$BASELINE_PATH"
      else
        run_scenario "$label" "$PROMPT_DIR/$s.txt" "$CLAUDE_SETTINGS" "$PIXEL_PATH"
      fi
      ms=$(cat "$TMPDIR_M/$label.ms")
      counts=$(cat "$TMPDIR_M/$label.counts")
      tool_calls=$(echo "$counts" | cut -d' ' -f1)
      turns=$(echo "$counts" | cut -d' ' -f2)
      echo "  $label: ${ms}ms tools=${tool_calls} turns=${turns}" >> "$RESULTS"
    done
  done
done

# --- Pixel-usage check, recorded into the results file ---
echo "" >> "$RESULTS"
echo "=== PIXEL USAGE CHECK ===" >> "$RESULTS"
for s in $SCENARIOS; do
  for i in $(seq 1 "$N"); do
    label="pixel-${s}-${i}"
    used=$(grep -cE "pixel (search|resolve|targets|reconcile|excavate|rescue)" "$OUTDIR/$label.json" 2>/dev/null || echo "0")
    if [ "$used" -gt 0 ]; then
      echo "  $label: pixel used" >> "$RESULTS"
    else
      echo "  $label: pixel NOT used (fell back to grep/git)" >> "$RESULTS"
    fi
  done
done

echo "=== done ===" >> "$RESULTS"

# Print summary table (mean over N runs)
echo ""
echo "=== RESULTS ==="
cat "$RESULTS"

echo ""
echo "=== COMPARISON (mean over $N runs) ==="
printf "%-25s %12s %12s %8s %10s %10s\n" "Scenario" "Baseline" "WithPixel" "Delta" "Tools" "Turns"
printf "%-25s %12s %12s %8s %10s %10s\n" "-------------------------" "------------" "------------" "--------" "----------" "----------"
for s in $SCENARIOS; do
  b=$(python3 - "$s" "$N" "$TMPDIR_M" baseline << 'PY'
import sys, statistics, os
s, n, d, arm = sys.argv[1], int(sys.argv[2]), sys.argv[3], sys.argv[4]
vals = []
for i in range(1, n + 1):
    p = os.path.join(d, f"{arm}-{s}-{i}.ms")
    if os.path.exists(p):
        try: vals.append(int(open(p).read().strip()))
        except Exception: pass
print(int(statistics.mean(vals)) if vals else 0)
PY
)
  p=$(python3 - "$s" "$N" "$TMPDIR_M" pixel << 'PY'
import sys, statistics, os
s, n, d, arm = sys.argv[1], int(sys.argv[2]), sys.argv[3], sys.argv[4]
vals = []
for i in range(1, n + 1):
    p = os.path.join(d, f"{arm}-{s}-{i}.ms")
    if os.path.exists(p):
        try: vals.append(int(open(p).read().strip()))
        except Exception: pass
print(int(statistics.mean(vals)) if vals else 0)
PY
)
  t=$(python3 - "$s" "$N" "$TMPDIR_M" pixel << 'PY'
import sys, statistics, os
s, n, d, arm = sys.argv[1], int(sys.argv[2]), sys.argv[3], sys.argv[4]
vals = []
for i in range(1, n + 1):
    p = os.path.join(d, f"{arm}-{s}-{i}.counts")
    if os.path.exists(p):
        try:
            tc, _ = open(p).read().split()
            vals.append(int(tc))
        except Exception: pass
print(int(statistics.mean(vals)) if vals else 0)
PY
)
  u=$(python3 - "$s" "$N" "$TMPDIR_M" pixel << 'PY'
import sys, statistics, os
s, n, d, arm = sys.argv[1], int(sys.argv[2]), sys.argv[3], sys.argv[4]
vals = []
for i in range(1, n + 1):
    p = os.path.join(d, f"{arm}-{s}-{i}.counts")
    if os.path.exists(p):
        try:
            _, tu = open(p).read().split()
            vals.append(int(tu))
        except Exception: pass
print(int(statistics.mean(vals)) if vals else 0)
PY
)
  if [ "$b" -gt 0 ] 2>/dev/null; then
    delta=$(( (p - b) * 100 / b ))
    printf "%-25s %10sms %10sms %7d%% %10d %10d\n" "$s" "$b" "$p" "$delta" "$t" "$u"
  fi
done
