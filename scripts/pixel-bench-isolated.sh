#!/usr/bin/env bash
# pixel-bench-isolated.sh — isolate PIXEL'S OWN cost from the rest of the
# user's global CLAUDE.md.
#
# pixel-bench.sh's "pixel" arm loads the user's ENTIRE global CLAUDE.md
# (~140KB / ~35K tokens across dozens of unrelated rules — RTK, Jira, Comet
# browser automation, credential policy, etc.) because settings.json cannot
# select which memory content loads; only --safe-mode/--bare can suppress
# CLAUDE.md, and both of those also kill the PreToolUse hooks pixel relies on.
# That makes pixel-bench.sh's comparison "zero customization vs this user's
# entire agent config stack" — not "with pixel vs without pixel". Measured
# 2026-08-30: pixel's own doctrine is ~4.6KB (~1.2K tokens) after trimming;
# the other ~130KB belongs to unrelated rules.
#
# This script isolates pixel's OWN prompt/behavior cost: `claude --safe-mode
# --append-system-prompt "$(cat pixel.md)"` gives an agent with pixel's
# doctrine as its ONLY instructions and pixel on PATH, but (like the
# pixel-free baseline) no PreToolUse guard hook — --safe-mode disables hooks
# too (verified 2026-08-30: `git pull` under --safe-mode is NOT blocked).
# So this measures "pixel's doctrine text + the agent choosing to call the
# pixel binary voluntarily" — NOT the full product (doctrine + mechanical
# guard enforcement). Read alongside pixel-bench.sh's numbers, not instead of
# them: this answers "is pixel's own overhead the problem", that one answers
# "is the user's full config stack + pixel faster than nothing at all".
#
# Usage: scripts/pixel-bench-isolated.sh [N reps, default 1]

set -uo pipefail

REPO="${REPO:-$HOME/Documents/pixel}"
PIXEL_BIN="${PIXEL_BIN:-$HOME/.local/bin/pixel}"
RULE_FILE="${RULE_FILE:-$HOME/.agent-config/rules/pixel.md}"
N="${1:-1}"
OUTDIR="/tmp/pixel-bench-isolated-outputs"
TMPDIR_M="/tmp/pixel-bench-isolated-tmp"
RESULTS="$(pwd)/docs/bench/pixel-bench-isolated-results.txt"
mkdir -p "$OUTDIR" "$TMPDIR_M"
rm -f "$TMPDIR_M"/*.json "$TMPDIR_M"/*.ms "$TMPDIR_M"/*.counts

cd "$REPO"

if [ ! -x "$PIXEL_BIN" ]; then
  echo "ERROR: pixel binary not found at $PIXEL_BIN." >&2
  exit 1
fi
if [ ! -f "$RULE_FILE" ]; then
  echo "ERROR: rule file not found at $RULE_FILE." >&2
  exit 1
fi

PIXEL_DOCTRINE="$(cat "$RULE_FILE")"
PIXEL_DIR="$(dirname "$PIXEL_BIN")"
PIXEL_PATH="$PIXEL_DIR:$PATH"

PROMPT_DIR="/tmp/pixel-bench-prompts"
if [ ! -f "$PROMPT_DIR/s1-locate.txt" ]; then
  echo "ERROR: prompt files not found at $PROMPT_DIR — run scripts/pixel-bench.sh once first" \
       "(it writes these), or copy them manually." >&2
  exit 1
fi

echo "preflight: verifying claude -p produces a successful result event..."
PREFLIGHT_OUT="$TMPDIR_M/preflight.json"
printf 'Reply with the single word OK and do nothing else.' | \
  claude --safe-mode -p --dangerously-skip-permissions --verbose \
    --output-format stream-json \
    > "$PREFLIGHT_OUT" 2>&1 || true
if ! grep -q '"type":"result"' "$PREFLIGHT_OUT" \
   || ! grep -q '"subtype":"success"' "$PREFLIGHT_OUT" \
   || grep -q 'Not logged in' "$PREFLIGHT_OUT" \
   || ! grep -q '"type":"assistant"' "$PREFLIGHT_OUT"; then
  echo "ERROR: preflight claude run failed." >&2
  tail -5 "$PREFLIGHT_OUT" >&2
  exit 2
fi
echo "preflight: OK"

run_cell() {
  local label="$1"
  local prompt_file="$2"
  local arm="$3"  # "vanilla" | "pixel-isolated"
  local start end ms
  start=$(python3 -c 'import time; print(int(time.time()*1000))')
  if [ "$arm" = "pixel-isolated" ]; then
    PATH="$PIXEL_PATH" claude --safe-mode -p --dangerously-skip-permissions --verbose \
      --append-system-prompt "$PIXEL_DOCTRINE" \
      --output-format stream-json \
      < "$prompt_file" > "$OUTDIR/${label}.json" 2>&1 || true
  else
    claude --safe-mode -p --dangerously-skip-permissions --verbose \
      --output-format stream-json \
      < "$prompt_file" > "$OUTDIR/${label}.json" 2>&1 || true
  fi
  end=$(python3 -c 'import time; print(int(time.time()*1000))')
  ms=$((end - start))
  echo "${ms}" > "$TMPDIR_M/${label}.ms"
  python3 - "$OUTDIR/${label}.json" "$TMPDIR_M/${label}.counts" << 'PY'
import json, sys
path, out = sys.argv[1], sys.argv[2]
tool_calls = 0
turns = 0
valid = 0
api_ms = 0
try:
    with open(path) as f:
        for line in f:
            try:
                ev = json.loads(line)
            except Exception:
                continue
            if ev.get("type") == "assistant":
                turns += 1
                msg = ev.get("message") or {}
                for blk in (msg.get("content") or []):
                    if isinstance(blk, dict) and blk.get("type") == "tool_use":
                        tool_calls += 1
            if ev.get("type") == "result":
                if ev.get("subtype") == "success":
                    valid = 1
                api_ms = ev.get("duration_api_ms") or ev.get("duration_ms") or 0
except FileNotFoundError:
    pass
with open(out, "w") as f:
    f.write(f"{tool_calls} {turns} {valid} {api_ms}\n")
PY
}

pixel_invocation_count() {
  python3 - "$OUTDIR/$1.json" << 'PY'
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
}

echo "=== pixel-bench-isolated: claude --safe-mode, pixel doctrine only vs nothing ===" > "$RESULTS"
echo "Date: $(date -u +%Y-%m-%dT%H:%M:%SZ)" >> "$RESULTS"
echo "Reps per cell: $N" >> "$RESULTS"
echo "Rule file: $RULE_FILE ($(wc -c < "$RULE_FILE") bytes)" >> "$RESULTS"
echo "NOTE: neither arm has the PreToolUse guard hook (--safe-mode disables hooks)." >> "$RESULTS"
echo "This isolates pixel's DOCTRINE TEXT + voluntary tool use, not the full product." >> "$RESULTS"

SCENARIOS="s1-locate s2-scope s3-sync s4-recover"
for s in $SCENARIOS; do
  echo "" >> "$RESULTS"
  echo "--- scenario $s ---" >> "$RESULTS"
  ARM_ORDER="vanilla pixel-isolated"
  [ $((RANDOM % 2)) -eq 0 ] && ARM_ORDER="pixel-isolated vanilla"
  for arm in $ARM_ORDER; do
    for i in $(seq 1 "$N"); do
      label="${arm}-${s}-${i}"
      run_cell "$label" "$PROMPT_DIR/$s.txt" "$arm"
      ms=$(cat "$TMPDIR_M/$label.ms" 2>/dev/null || echo "?")
      read -r tool_calls turns valid api_ms < "$TMPDIR_M/$label.counts"
      if [ "${valid:-0}" = "1" ]; then
        pixel_uses=$(pixel_invocation_count "$label")
        echo "  $label: ${ms}ms (api=${api_ms}ms) tools=${tool_calls} turns=${turns} pixel_calls=${pixel_uses}" >> "$RESULTS"
      else
        echo "  $label: INVALID (no successful result event) — excluded" >> "$RESULTS"
      fi
    done
  done
done

echo "" >> "$RESULTS"
echo "=== RESULTS ===" >> "$RESULTS"
cat "$RESULTS"

echo ""
echo "=== COMPARISON (mean over valid runs) ==="
python3 - "$N" $SCENARIOS << 'PY'
import sys, os, statistics, json
N = int(sys.argv[1])
scenarios = sys.argv[2:]
tmp = "/tmp/pixel-bench-isolated-tmp"
print(f"{'Scenario':<14}{'Vanilla':>12}{'Isolated pixel':>18}{'Delta':>10}")
for s in scenarios:
    def cell(arm):
        vals = []
        for i in range(1, N + 1):
            label = f"{arm}-{s}-{i}"
            msf = os.path.join(tmp, f"{label}.ms")
            cf = os.path.join(tmp, f"{label}.counts")
            if not (os.path.exists(msf) and os.path.exists(cf)):
                continue
            with open(cf) as f:
                parts = f.read().split()
            if len(parts) >= 3 and parts[2] == "1":
                vals.append(int(open(msf).read().strip()))
        return vals
    a = cell("vanilla")
    b = cell("pixel-isolated")
    if not a or not b:
        print(f"{s:<14}{'n/a':>12}{'n/a':>18}{'n/a':>10}")
        continue
    ma, mb = statistics.mean(a), statistics.mean(b)
    delta = (mb - ma) / ma * 100 if ma else 0
    print(f"{s:<14}{ma/1000:>10.1f}s{mb/1000:>16.1f}s{delta:>+9.0f}%")
PY
