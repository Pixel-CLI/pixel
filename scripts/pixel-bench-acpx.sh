#!/usr/bin/env bash
# acpx-based pixel benchmark with parallel execution and result caching.
# - All 4 scenarios x 2 arms launched in parallel (8 concurrent acpx sessions)
# - Results cached by (commit, scenario, arm, rep) — skip if unchanged
# - Per-tool-call timing via timed pixel wrapper
# - Baseline: pixel excluded from PATH, stripped CLAUDE.md/AGENTS.md
set -euo pipefail

REPO="${1:-$HOME/Documents/pixel}"
PIXEL_BIN="${PIXEL_BIN:-$HOME/.local/bin/pixel}"
N="${N:-3}"
OUTDIR="/tmp/acpx-bench-outputs"
CACHE_DIR="/tmp/acpx-bench-cache"
TMPDIR_M="/tmp/acpx-bench-tmp"
RESULTS="${RESULTS:-$(pwd)/acpx-bench-results.txt}"
mkdir -p "$OUTDIR" "$TMPDIR_M" "$CACHE_DIR"

cd "$REPO"
COMMIT=$(git rev-parse --short HEAD)

# --- Timed pixel wrapper ---
mkdir -p /tmp/pixel-timed-bin
cat > /tmp/pixel-timed-bin/pixel << 'EOF'
#!/bin/sh
start=$(python3 -c 'import time; print(int(time.time()*1000))')
"$HOME/.local/bin/pixel" "$@" 2>&1
rc=$?
end=$(python3 -c 'import time; print(int(time.time()*1000))')
echo "[pixel] $((end - start))ms: pixel $*" >> "${PIXEL_TIMING_LOG:-/tmp/pixel-timings-default.log}"
exit $rc
EOF
chmod +x /tmp/pixel-timed-bin/pixel

# --- Build a pixel-free CLAUDE.md for baseline ---
BASELINE_CLAUDE="/tmp/acpx-bench-baseline-CLAUDE.md"
if [ -f "$REPO/CLAUDE.md" ]; then
  python3 - "$REPO/CLAUDE.md" "$BASELINE_CLAUDE" << 'PY'
import sys
src, dst = sys.argv[1], sys.argv[2]
with open(src) as f:
    text = f.read()
lines = text.split('\n')
out = []
skip = False
for line in lines:
    if 'Reinstall and Reconfig' in line and line.startswith('#'):
        skip = True
        continue
    if skip and line.startswith('# ') and 'Reinstall' not in line:
        skip = False
    if skip and line.startswith('## ') and 'Reinstall' not in line:
        skip = False
    if not skip:
        out.append(line)
with open(dst, 'w') as f:
    f.write('\n'.join(out).strip() + '\n')
PY
else
  echo "# No project rules" > "$BASELINE_CLAUDE"
fi

# --- Prompts ---
PROMPT_DIR="/tmp/acpx-bench-prompts"
mkdir -p "$PROMPT_DIR"
cat > "$PROMPT_DIR/s1-locate.txt" << 'PROMPT'
You are working in a Rust CLI tool repository. Find where GUARD_MATCHER is defined and show its full definition with surrounding context. Report the file path, line number, and the full definition.
PROMPT
cat > "$PROMPT_DIR/s2-scope.txt" << 'PROMPT'
You are working in a Rust CLI tool repository. I want to add a new agent tool called "foobar" to the guard matcher. Find ALL files that would need to be modified for this change. List each file with a brief reason.
PROMPT
cat > "$PROMPT_DIR/s3-sync.txt" << 'PROMPT'
You are working in a Rust CLI tool repository. Sync this branch with origin/main. Report what happened.
PROMPT
cat > "$PROMPT_DIR/s4-recover.txt" << 'PROMPT'
You are working in a Rust CLI tool repository. Find the deleted function register_mcp_server that was removed from the codebase. Show the commit that removed it, the file it lived in, and the full original implementation.
PROMPT

SCENARIOS="${SCENARIOS:-s1-locate s2-scope s3-sync s4-recover}"

# --- Ensure indexed ---
"$PIXEL_BIN" index "$REPO" 2>/dev/null || true
"$PIXEL_BIN" index --history "$REPO" 2>/dev/null || true

# --- Run one cell via acpx (background-safe) ---
run_cell() {
  local label="$1"
  local prompt_file="$2"
  local use_pixel="$3"
  local cache_key="${COMMIT}-${label}"
  local cache_file="$CACHE_DIR/${cache_key}.done"
  local timing_log="$TMPDIR_M/${label}.pixel-timings"
  local cmds_log="$TMPDIR_M/${label}.cmds"
  local ms_file="$TMPDIR_M/${label}.ms"
  local output_file="$OUTDIR/${label}.txt"

  # Cache check: skip if already done for this commit
  if [ -f "$cache_file" ] && [ -f "$ms_file" ] && [ -f "$cmds_log" ] && [ -f "$output_file" ]; then
    echo "[cache] $label: already done for $COMMIT"
    return 0
  fi

  rm -f "$timing_log" "$ms_file" "$cmds_log"

  local start end ms
  start=$(python3 -c 'import time; print(int(time.time()*1000))')

  if [ "$use_pixel" = "1" ]; then
    PIXEL_TIMING_LOG="$timing_log" \
    PATH="/tmp/pixel-timed-bin:$PATH" \
      acpx --model sonnet --approve-all --cwd "$REPO" claude exec \
        -f "$prompt_file" \
        > "$output_file" 2>&1 || true
  else
    # Baseline: pixel excluded from PATH. CLAUDE.md/AGENTS.md already
    # swapped to stripped version before parallel launch (see below).
    local clean_path
    clean_path=$(echo "$PATH" | tr ':' '\n' | grep -v 'pixel-timed-bin' | grep -v "$HOME/.local/bin" | tr '\n' ':' | sed 's/:$//')
    PATH="$clean_path" \
      acpx --model sonnet --approve-all --cwd "$REPO" claude exec \
        -f "$prompt_file" \
        > "$output_file" 2>&1 || true
  fi

  end=$(python3 -c 'import time; print(int(time.time()*1000))')
  ms=$((end - start))
  echo "$ms" > "$ms_file"

  # Extract tool call counts
  python3 - "$output_file" "$cmds_log" << 'PY'
import sys
path, out = sys.argv[1], sys.argv[2]
tool_calls = 0
pixel_calls = 0
git_calls = 0
read_calls = 0
cmds = []
with open(path) as f:
    for line in f:
        if '[tool]' in line and '(completed)' in line:
            tool_calls += 1
            cmd = line.replace('[tool] ', '').rsplit(' (completed)', 1)[0].strip()
            cmds.append(cmd)
            if cmd.startswith('pixel '):
                pixel_calls += 1
            elif cmd.startswith('git ') or cmd.startswith('rtk git'):
                git_calls += 1
            elif cmd.startswith('Read ') or cmd.startswith('rtk read'):
                read_calls += 1
with open(out, 'w') as f:
    f.write(f"{tool_calls} {pixel_calls} {git_calls} {read_calls}\n")
    for c in cmds:
        f.write(c + '\n')
PY

  # Mark cache done
  echo "$ms" > "$cache_file"
  echo "[done] $label: ${ms}ms"
}

# --- Launch all cells in parallel (two waves to avoid CLAUDE.md races) ---
echo "=== acpx pixel bench (parallel) ==="
echo "Commit: $COMMIT"
echo "Reps: $N"

# Check which cells need running
NEEDS_BASELINE=0
NEEDS_PIXEL=0
for s in $SCENARIOS; do
  for i in $(seq 1 "$N"); do
    [ -f "$CACHE_DIR/${COMMIT}-baseline-${s}-${i}.done" ] || NEEDS_BASELINE=1
    [ -f "$CACHE_DIR/${COMMIT}-pixel-${s}-${i}.done" ] || NEEDS_PIXEL=1
  done
done

echo "Launching cells (baseline wave + pixel wave)..."
echo ""

# --- Wave 1: all baseline cells in parallel (stripped CLAUDE.md) ---
if [ "$NEEDS_BASELINE" = "1" ]; then
  cp "$REPO/CLAUDE.md" "$TMPDIR_M/CLAUDE.md.orig" 2>/dev/null || true
  cp "$REPO/AGENTS.md" "$TMPDIR_M/AGENTS.md.orig" 2>/dev/null || true
  cp "$BASELINE_CLAUDE" "$REPO/CLAUDE.md"
  cp "$BASELINE_CLAUDE" "$REPO/AGENTS.md"
  echo "Wave 1: baseline (pixel-free CLAUDE.md, no pixel on PATH)"

  PIDS=""
  for s in $SCENARIOS; do
    for i in $(seq 1 "$N"); do
      run_cell "baseline-${s}-${i}" "$PROMPT_DIR/$s.txt" "0" &
      PIDS="$PIDS $!"
    done
  done
  for pid in $PIDS; do wait "$pid" 2>/dev/null || true; done
  echo "Wave 1 complete."
fi

# --- Wave 2: all pixel cells in parallel (real CLAUDE.md, pixel on PATH) ---
if [ "$NEEDS_PIXEL" = "1" ]; then
  if [ -f "$TMPDIR_M/CLAUDE.md.orig" ]; then
    cp "$TMPDIR_M/CLAUDE.md.orig" "$REPO/CLAUDE.md"
    cp "$TMPDIR_M/AGENTS.md.orig" "$REPO/AGENTS.md"
  fi
  echo "Wave 2: pixel (real CLAUDE.md, pixel on PATH, guard hooks active)"

  PIDS=""
  for s in $SCENARIOS; do
    for i in $(seq 1 "$N"); do
      run_cell "pixel-${s}-${i}" "$PROMPT_DIR/$s.txt" "1" &
      PIDS="$PIDS $!"
    done
  done
  for pid in $PIDS; do wait "$pid" 2>/dev/null || true; done
  echo "Wave 2 complete."
fi

# Restore real CLAUDE.md/AGENTS.md
if [ -f "$TMPDIR_M/CLAUDE.md.orig" ]; then
  cp "$TMPDIR_M/CLAUDE.md.orig" "$REPO/CLAUDE.md" 2>/dev/null || true
  cp "$TMPDIR_M/AGENTS.md.orig" "$REPO/AGENTS.md" 2>/dev/null || true
fi

# --- Generate results ---
echo "=== acpx pixel bench ===" > "$RESULTS"
echo "Date: $(date -u +%Y-%m-%dT%H:%M:%SZ)" >> "$RESULTS"
echo "Repo: $COMMIT ($REPO)" >> "$RESULTS"
echo "Pixel: $($PIXEL_BIN --version 2>/dev/null || echo 'not built')" >> "$RESULTS"
echo "Reps per cell: $N" >> "$RESULTS"
echo "" >> "$RESULTS"

for s in $SCENARIOS; do
  echo "--- scenario $s ---" >> "$RESULTS"
  for arm in baseline pixel; do
    for i in $(seq 1 "$N"); do
      label="${arm}-${s}-${i}"
      ms=$(cat "$TMPDIR_M/${label}.ms" 2>/dev/null || echo "0")
      counts=$(head -1 "$TMPDIR_M/${label}.cmds" 2>/dev/null || echo "0 0 0 0")
      tc=$(echo "$counts" | cut -d' ' -f1)
      pc=$(echo "$counts" | cut -d' ' -f2)
      gc=$(echo "$counts" | cut -d' ' -f3)
      rc=$(echo "$counts" | cut -d' ' -f4)
      echo "  $label: ${ms}ms  tools=$tc pixel=$pc git=$gc read=$rc" >> "$RESULTS"
    done
  done
done

# --- Pixel timing breakdown ---
echo "" >> "$RESULTS"
echo "=== PIXEL CALL TIMINGS ===" >> "$RESULTS"
for s in $SCENARIOS; do
  for i in $(seq 1 "$N"); do
    label="pixel-${s}-${i}"
    tlog="$TMPDIR_M/${label}.pixel-timings"
    if [ -f "$tlog" ] && [ -s "$tlog" ]; then
      total_pixel=$(python3 -c "
import re
total = 0
with open('$tlog') as f:
    for line in f:
        m = re.search(r'\[(\d+)ms\]', line)
        if m: total += int(m.group(1))
print(total)
" 2>/dev/null || echo "0")
      echo "  $label: pixel_total=${total_pixel}ms" >> "$RESULTS"
      sed 's/^/    /' "$tlog" >> "$RESULTS"
    fi
  done
done

# --- Comparison table ---
echo "" >> "$RESULTS"
echo "=== COMPARISON (mean over $N runs) ===" >> "$RESULTS"
printf "%-20s %10s %10s %8s %8s %8s %10s\n" "Scenario" "Base_ms" "Pixel_ms" "Delta%" "B_tools" "P_tools" "P_px_ms" >> "$RESULTS"
printf "%-20s %10s %10s %8s %8s %8s %10s\n" "--------------------" "----------" "----------" "--------" "--------" "--------" "----------" >> "$RESULTS"
for s in $SCENARIOS; do
  python3 - "$s" "$N" "$TMPDIR_M" >> "$RESULTS" << 'PY'
import sys, os, statistics, re
s, n, d = sys.argv[1], int(sys.argv[2]), sys.argv[3]
def mean_ms(arm):
    vals = []
    for i in range(1, n+1):
        p = os.path.join(d, f"{arm}-{s}-{i}.ms")
        if os.path.exists(p):
            try: vals.append(int(open(p).read().strip()))
            except: pass
    return int(statistics.mean(vals)) if vals else 0
def mean_tools(arm):
    vals = []
    for i in range(1, n+1):
        p = os.path.join(d, f"{arm}-{s}-{i}.cmds")
        if os.path.exists(p):
            try: vals.append(int(open(p).readline().split()[0]))
            except: pass
    return int(statistics.mean(vals)) if vals else 0
def pixel_total_mean():
    vals = []
    for i in range(1, n+1):
        tlog = os.path.join(d, f"pixel-{s}-{i}.pixel-timings")
        if os.path.exists(tlog):
            total = 0
            with open(tlog) as f:
                for line in f:
                    m = re.search(r'\[(\d+)ms\]', line)
                    if m: total += int(m.group(1))
            vals.append(total)
    return int(statistics.mean(vals)) if vals else 0
b = mean_ms("baseline")
p = mean_ms("pixel")
bt = mean_tools("baseline")
pt = mean_tools("pixel")
pxms = pixel_total_mean()
delta = ((p - b) * 100 // b) if b > 0 else 0
print(f"{s:<20} {b:>9}ms {p:>9}ms {delta:>7}% {bt:>8} {pt:>8} {pxms:>9}ms")
PY
done

echo "" >> "$RESULTS"
echo "=== done ===" >> "$RESULTS"

cat "$RESULTS"
