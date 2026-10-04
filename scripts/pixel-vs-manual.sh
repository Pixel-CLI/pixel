#!/usr/bin/env bash
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

# pixel-vs-manual.sh — direct comparison: pixel commands vs manual grep/git.
#
# Runs the SAME tasks two ways and shows timing + output quality side by side.
# No agent involved — just the raw commands, so the difference is clear.
#
# Usage:
#   scripts/pixel-vs-manual.sh                 # uses cwd as repo
#   scripts/pixel-vs-manual.sh /path/to/repo   # custom repo
#
# Prerequisites:
#   - pixel installed (`command -v pixel`), or built under target/ (see
#     CONTRIBUTING.md "Local install loop"); PIXEL_BIN overrides
#   - the repo is indexed on first run (pixel build-index + pixel rebuild-graph, below)

set -euo pipefail

REPO="${1:-$(pwd)}"
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

cd "$REPO"

if [ ! -x "$PIXEL_BIN" ]; then
  echo "ERROR: pixel binary not found at $PIXEL_BIN" >&2
  exit 1
fi

# ── Colors ──────────────────────────────────────────────────────────────
B='\033[1m'; R='\033[0m'; RED='\033[31m'; GRN='\033[32m'; YLW='\033[33m'
BLU='\033[34m'; CYA='\033[36m'; DIM='\033[2m'

# ── Timing helper ───────────────────────────────────────────────────────
time_ms() {
  python3 -c 'import time; print(int(time.time()*1000))'
}

# ── Run a command and capture timing + output ───────────────────────────
run_cmd() {
  local label="$1"; shift
  local outfile="$1"; shift
  local start end ms
  start=$(time_ms)
  "$@" > "$outfile" 2>&1 || true
  end=$(time_ms)
  ms=$((end - start))
  echo "$ms"
}

# ── Print a section header ──────────────────────────────────────────────
header() {
  echo ""
  echo -e "${B}${CYA}┏━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┓${R}"
  echo -e "${B}${CYA}┃  $1${R}"
  echo -e "${B}${CYA}┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┛${R}"
}

# ── Print a comparison row ──────────────────────────────────────────────
row() {
  local task="$1" manual_ms="$2" pixel_ms="$3"
  local delta=$((pixel_ms - manual_ms))
  local pct=0
  if [ "$manual_ms" -gt 0 ]; then
    pct=$((delta * 100 / manual_ms))
  fi
  local sign color
  if [ "$delta" -lt 0 ]; then
    sign="▼"; color="$GRN"
  else
    sign="▲"; color="$RED"
  fi
  printf "  %-35s ${DIM}%-8s${R} → ${color}%-8s${R}  ${color}%s%d%%${R}\n" \
    "$task" "${manual_ms}ms" "${pixel_ms}ms" "$sign" "$((pct < 0 ? -pct : pct))"
}

# ── Ensure repo is indexed ──────────────────────────────────────────────
echo -e "${DIM}Indexing repo...${R}"
"$PIXEL_BIN" build-index "$REPO" 2>/dev/null || true
"$PIXEL_BIN" rebuild-graph "$REPO" 2>/dev/null || true
echo ""

# `install` is ambiguous by bare name (a function, a module, a test method):
# impact/context take the uid, which `pixel find-symbol` prints in its last column.
INSTALL_UID=$("$PIXEL_BIN" find-symbol install "$REPO" 2>/dev/null | awk '$1=="function"{print $NF; exit}')
INSTALL_UID="${INSTALL_UID:-crates/pixel-install/src/install.rs#install#function}"

OUTDIR="/tmp/pixel-vs-manual"
mkdir -p "$OUTDIR"

# ════════════════════════════════════════════════════════════════════════
# HEADER
# ════════════════════════════════════════════════════════════════════════
echo -e "${B}${CYA}╔══════════════════════════════════════════════════════════════╗${R}"
echo -e "${B}${CYA}║   PIXEL vs MANUAL — direct command comparison               ║${R}"
echo -e "${B}${CYA}╚══════════════════════════════════════════════════════════════╝${R}"
echo ""
echo -e "  ${B}Repo:${R}  $REPO"
echo -e "  ${B}Pixel:${R} $($PIXEL_BIN --version)"
echo ""

# ════════════════════════════════════════════════════════════════════════
# TASK 1: Find a symbol definition
# ════════════════════════════════════════════════════════════════════════
header "TASK 1: Find where GUARD_MATCHER is defined"

echo -e "\n  ${YLW}${B}── MANUAL: grep -rn GUARD_MATCHER --include='*.rs' ──${R}"
M1=$(run_cmd "manual-1" "$OUTDIR/manual-1.txt" \
  grep -rn "GUARD_MATCHER" --include="*.rs" "$REPO" 2>/dev/null || true)
head -10 "$OUTDIR/manual-1.txt" | sed 's/^/  /'
echo -e "  ${DIM}... ($(wc -l < "$OUTDIR/manual-1.txt") lines total)${R}"
echo -e "  ${DIM}Time: ${M1}ms${R}"

echo -e "\n  ${GRN}${B}── PIXEL: pixel search-content GUARD_MATCHER ──${R}"
P1=$(run_cmd "pixel-1" "$OUTDIR/pixel-1.txt" \
  "$PIXEL_BIN" search-content "GUARD_MATCHER" "$REPO" 2>/dev/null || true)
head -10 "$OUTDIR/pixel-1.txt" | sed 's/^/  /'
echo -e "  ${DIM}... ($(wc -l < "$OUTDIR/pixel-1.txt") lines total)${R}"
echo -e "  ${DIM}Time: ${P1}ms${R}"

row "Find symbol definition" "$M1" "$P1"

# ════════════════════════════════════════════════════════════════════════
# TASK 2: Find all callers of a function (impact analysis)
# ════════════════════════════════════════════════════════════════════════
header "TASK 2: Find all callers of install() (impact analysis)"

echo -e "\n  ${YLW}${B}── MANUAL: grep + manual trace ──${R}"
M2=$(run_cmd "manual-2" "$OUTDIR/manual-2.txt" \
  bash -c "grep -rn 'install(' --include='*.rs' "$REPO" | head -30" 2>/dev/null || true)
head -10 "$OUTDIR/manual-2.txt" | sed 's/^/  /'
echo -e "  ${DIM}... ($(wc -l < "$OUTDIR/manual-2.txt") lines total)${R}"
echo -e "  ${DIM}Time: ${M2}ms${R}"

echo -e "\n  ${GRN}${B}── PIXEL: pixel impact $INSTALL_UID ──${R}"
P2=$(run_cmd "pixel-2" "$OUTDIR/pixel-2.txt" \
  "$PIXEL_BIN" impact "$INSTALL_UID" "$REPO" 2>/dev/null || true)
head -20 "$OUTDIR/pixel-2.txt" | sed 's/^/  /'
echo -e "  ${DIM}... ($(wc -l < "$OUTDIR/pixel-2.txt") lines total)${R}"
echo -e "  ${DIM}Time: ${P2}ms${R}"

row "Impact analysis (callers)" "$M2" "$P2"

# ════════════════════════════════════════════════════════════════════════
# TASK 3: Search deleted code in git history
# ════════════════════════════════════════════════════════════════════════
header "TASK 3: Find deleted function in git history"

# Pick a function that was actually deleted/modified
DELETED_FN="register_mcp_server"

echo -e "\n  ${YLW}${B}── MANUAL: git log -p | grep register_mcp_server ──${R}"
M3=$(run_cmd "manual-3" "$OUTDIR/manual-3.txt" \
  bash -c "git log -p --all -S '$DELETED_FN' -- '*.rs' 2>/dev/null | head -50" || true)
head -15 "$OUTDIR/manual-3.txt" | sed 's/^/  /'
echo -e "  ${DIM}... ($(wc -l < "$OUTDIR/manual-3.txt") lines total)${R}"
echo -e "  ${DIM}Time: ${M3}ms${R}"

echo -e "\n  ${GRN}${B}── PIXEL: pixel dig-history --phrase register_mcp_server ──${R}"
P3=$(run_cmd "pixel-3" "$OUTDIR/pixel-3.txt" \
  "$PIXEL_BIN" dig-history --phrase "$DELETED_FN" "$REPO" 2>/dev/null || true)
head -15 "$OUTDIR/pixel-3.txt" | sed 's/^/  /'
echo -e "  ${DIM}... ($(wc -l < "$OUTDIR/pixel-3.txt") lines total)${R}"
echo -e "  ${DIM}Time: ${P3}ms${R}"

row "Search deleted code in history" "$M3" "$P3"

# ════════════════════════════════════════════════════════════════════════
# TASK 4: Resolve a concept/phrase to code
# ════════════════════════════════════════════════════════════════════════
header "TASK 4: Resolve 'guard matcher' to actual code"

echo -e "\n  ${YLW}${B}── MANUAL: grep -ri 'guard matcher' ──${R}"
M4=$(run_cmd "manual-4" "$OUTDIR/manual-4.txt" \
  bash -c "grep -rni 'guard.matcher' --include='*.rs' "$REPO" | head -20" 2>/dev/null || true)
head -10 "$OUTDIR/manual-4.txt" | sed 's/^/  /'
echo -e "  ${DIM}... ($(wc -l < "$OUTDIR/manual-4.txt") lines total)${R}"
echo -e "  ${DIM}Time: ${M4}ms${R}"

echo -e "\n  ${GRN}${B}── PIXEL: pixel find-code 'guard matcher' ──${R}"
P4=$(run_cmd "pixel-4" "$OUTDIR/pixel-4.txt" \
  "$PIXEL_BIN" find-code "guard matcher" "$REPO" 2>/dev/null || true)
head -15 "$OUTDIR/pixel-4.txt" | sed 's/^/  /'
echo -e "  ${DIM}... ($(wc -l < "$OUTDIR/pixel-4.txt") lines total)${R}"
echo -e "  ${DIM}Time: ${P4}ms${R}"

row "Resolve concept to code" "$M4" "$P4"

# ════════════════════════════════════════════════════════════════════════
# TASK 5: Get context for a symbol (what does this function do?)
# ════════════════════════════════════════════════════════════════════════
header "TASK 5: Get context for a symbol"

# A symbol uid is `<path>#<name>#<kind>` (resolved above).
SYMBOL_UID="$INSTALL_UID"

echo -e "\n  ${YLW}${B}── MANUAL: find + read the file ──${R}"
M5=$(run_cmd "manual-5" "$OUTDIR/manual-5.txt" \
  bash -c "grep -rn 'fn install' --include='*.rs' "$REPO" | head -5 && echo '---' && F=\$(grep -rl 'fn install' --include='*.rs' "$REPO" | head -1) && head -50 \$F" 2>/dev/null || true)
head -15 "$OUTDIR/manual-5.txt" | sed 's/^/  /'
echo -e "  ${DIM}... ($(wc -l < "$OUTDIR/manual-5.txt") lines total)${R}"
echo -e "  ${DIM}Time: ${M5}ms${R}"

echo -e "\n  ${GRN}${B}── PIXEL: pixel pack-context $SYMBOL_UID ──${R}"
P5=$(run_cmd "pixel-5" "$OUTDIR/pixel-5.txt" \
  "$PIXEL_BIN" pack-context "$SYMBOL_UID" "$REPO" 2>/dev/null || true)
head -15 "$OUTDIR/pixel-5.txt" | sed 's/^/  /'
echo -e "  ${DIM}... ($(wc -l < "$OUTDIR/pixel-5.txt") lines total)${R}"
echo -e "  ${DIM}Time: ${P5}ms${R}"

row "Get symbol context" "$M5" "$P5"

# ════════════════════════════════════════════════════════════════════════
# SUMMARY
# ════════════════════════════════════════════════════════════════════════
echo ""
echo -e "${B}${CYA}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${R}"
echo -e "${B}${CYA}                      SUMMARY                                 ${R}"
echo -e "${B}${CYA}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${R}"
echo ""
printf "  %-35s ${DIM}%-10s${R}  %-10s  %s\n" "Task" "Manual" "Pixel" "Delta"
printf "  %-35s %-10s  %-10s  %s\n" "──────────────────────────────────" "──────────" "──────────" "──────────"
row "1. Find symbol definition" "$M1" "$P1"
row "2. Impact analysis (callers)" "$M2" "$P2"
row "3. Search deleted code" "$M3" "$P3"
row "4. Resolve concept to code" "$M4" "$P4"
row "5. Get symbol context" "$M5" "$P5"
echo ""
echo -e "  ${DIM}Full outputs: $OUTDIR/{manual,pixel}-*.txt${R}"
echo ""

# Total time
TOTAL_M=$((M1 + M2 + M3 + M4 + M5))
TOTAL_P=$((P1 + P2 + P3 + P4 + P5))
TOTAL_DELTA=$((TOTAL_P - TOTAL_M))
echo -e "  ${B}Total manual:${R}  ${TOTAL_M}ms"
echo -e "  ${B}Total pixel:${R}   ${TOTAL_P}ms"
if [ "$TOTAL_DELTA" -lt 0 ]; then
  echo -e "  ${GRN}${B}Pixel saved $((TOTAL_DELTA * -1))ms overall${R}"
else
  echo -e "  ${RED}${B}Pixel added ${TOTAL_DELTA}ms overall${R}"
fi
echo ""
echo -e "  ${DIM}Note: pixel's advantage is not just speed — it returns${R}"
echo -e "  ${DIM}ranked, context-aware results with boundaries, while grep${R}"
echo -e "  ${DIM}returns raw lines that need manual interpretation.${R}"
echo ""
