#!/usr/bin/env bash
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

# pixel-excavate-demo.sh — excavation demo on ShipFast: find the Kimi model feature.
#
# Shows the same task done two ways:
#   1. MANUAL: git log -S + git show (the traditional approach)
#   2. PIXEL: pixel dig-history (indexed history search)
#
# The task: "Find where and how the Kimi model was plugged into the homepage engine"
#
# Usage:
#   scripts/pixel-excavate-demo.sh                 # uses ~/Documents/ship-fast
#   scripts/pixel-excavate-demo.sh /path/to/repo   # custom repo
#   PHRASE=stripe scripts/pixel-excavate-demo.sh /path/to/repo
#
# Prerequisites: pixel installed (or PIXEL_BIN), and the target repo's
# history indexed once: `pixel build-index --history /path/to/repo`.

set -euo pipefail

REPO="${1:-$HOME/Documents/ship-fast}"
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
PHRASE="${PHRASE:-kimi}"
FEATURE="${FEATURE:-Kimi model plugged into homepage engine}"

if [ ! -d "$REPO/.git" ]; then
  echo "ERROR: $REPO is not a git repository (pass the repo path as the first argument)" >&2
  exit 1
fi
cd "$REPO"

if [ -z "$PIXEL_BIN" ] || [ ! -x "$PIXEL_BIN" ]; then
  echo "ERROR: pixel binary not found (set PIXEL_BIN, install pixel, or build it)" >&2
  exit 1
fi
if [ ! -f "$REPO/.pixel/history.db" ]; then
  echo "ERROR: $REPO has no history index; run: pixel build-index --history \"$REPO\"" >&2
  exit 1
fi

# ── Colors ──────────────────────────────────────────────────────────────
B='\033[1m'; R='\033[0m'; RED='\033[31m'; GRN='\033[32m'; YLW='\033[33m'
BLU='\033[34m'; CYA='\033[36m'; DIM='\033[2m'

time_ms() { python3 -c 'import time; print(int(time.time()*1000))'; }

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

header() {
  echo ""
  echo -e "${B}${CYA}┏━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┓${R}"
  echo -e "${B}${CYA}┃  $1${R}"
  echo -e "${B}${CYA}┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┛${R}"
}

OUTDIR="/tmp/pixel-excavate-demo"
mkdir -p "$OUTDIR"

# ════════════════════════════════════════════════════════════════════════
# HEADER
# ════════════════════════════════════════════════════════════════════════
echo ""
echo -e "${B}${CYA}╔══════════════════════════════════════════════════════════════╗${R}"
echo -e "${B}${CYA}║   EXCAVATION DEMO — find the Kimi model feature              ║${R}"
echo -e "${B}${CYA}╚══════════════════════════════════════════════════════════════╝${R}"
echo ""
echo -e "  ${B}Repo:${R}    $REPO"
echo -e "  ${B}Pixel:${R}   $($PIXEL_BIN --version)"
echo -e "  ${B}Task:${R}    Find where and how the Kimi model was plugged in"
echo -e "  ${B}Commits:${R} $(git log --oneline | wc -l | tr -d ' ')"
echo ""

# ════════════════════════════════════════════════════════════════════════
# TASK 1: Find all commits that mention "kimi"
# ════════════════════════════════════════════════════════════════════════
header "TASK 1: Find all commits mentioning 'kimi'"

echo -e "\n  ${YLW}${B}── MANUAL: git log --grep kimi --oneline ──${R}"
M1=$(run_cmd "m1" "$OUTDIR/m1.txt" \
  git log --grep="$PHRASE" --oneline --all 2>/dev/null)
head -15 "$OUTDIR/m1.txt" | sed 's/^/  /'
echo -e "  ${DIM}... ($(wc -l < "$OUTDIR/m1.txt") commits total)${R}"
echo -e "  ${DIM}Time: ${M1}ms${R}"

echo -e "\n  ${GRN}${B}── PIXEL: pixel dig-history --phrase kimi ──${R}"
P1=$(run_cmd "p1" "$OUTDIR/p1.txt" \
  "$PIXEL_BIN" dig-history --phrase "$PHRASE" . 2>/dev/null)
head -30 "$OUTDIR/p1.txt" | sed 's/^/  /'
echo -e "  ${DIM}... ($(wc -l < "$OUTDIR/p1.txt") lines total)${R}"
echo -e "  ${DIM}Time: ${P1}ms${R}"

# ════════════════════════════════════════════════════════════════════════
# TASK 2: Find the commit that wired Kimi as the primary homepage engine
# ════════════════════════════════════════════════════════════════════════
header "TASK 2: Find the commit that wired Kimi as primary homepage engine"

echo -e "\n  ${YLW}${B}── MANUAL: git log -S 'kimi' --oneline -- src/ ──${R}"
M2=$(run_cmd "m2" "$OUTDIR/m2.txt" \
  git log -S "$PHRASE" --oneline --all -- '*.ts' '*.tsx' '*.js' '*.mjs' 2>/dev/null)
head -15 "$OUTDIR/m2.txt" | sed 's/^/  /'
echo -e "  ${DIM}... ($(wc -l < "$OUTDIR/m2.txt") commits total)${R}"
echo -e "  ${DIM}Time: ${M2}ms${R}"

echo -e "\n  ${GRN}${B}── PIXEL: pixel dig-history --phrase 'kimi hybrid primary homepage' ──${R}"
P2=$(run_cmd "p2" "$OUTDIR/p2.txt" \
  "$PIXEL_BIN" dig-history --phrase "kimi hybrid primary homepage" . 2>/dev/null)
head -30 "$OUTDIR/p2.txt" | sed 's/^/  /'
echo -e "  ${DIM}... ($(wc -l < "$OUTDIR/p2.txt") lines total)${R}"
echo -e "  ${DIM}Time: ${P2}ms${R}"

# ════════════════════════════════════════════════════════════════════════
# TASK 3: Show the actual diff that added the Kimi model wiring
# ════════════════════════════════════════════════════════════════════════
header "TASK 3: Show the diff that wired Kimi into the homepage engine"

# Find the specific commit
KIMI_COMMIT=$(git log --oneline --all --grep="wire kimi hybrid as primary" | head -1 | awk '{print $1}')
if [ -z "$KIMI_COMMIT" ]; then
  KIMI_COMMIT=$(git log --oneline --all --grep="kimi" --grep="primary" --all-match | head -1 | awk '{print $1}')
fi

echo -e "\n  ${DIM}Target commit: ${KIMI_COMMIT:-not found}${R}"

if [ -n "$KIMI_COMMIT" ]; then
  echo -e "\n  ${YLW}${B}── MANUAL: git show <commit> --stat ──${R}"
  M3=$(run_cmd "m3" "$OUTDIR/m3.txt" \
    git show "$KIMI_COMMIT" --stat 2>/dev/null)
  head -20 "$OUTDIR/m3.txt" | sed 's/^/  /'
  echo -e "  ${DIM}... ($(wc -l < "$OUTDIR/m3.txt") lines total)${R}"
  echo -e "  ${DIM}Time: ${M3}ms${R}"

  echo -e "\n  ${GRN}${B}── PIXEL: pixel dig-history --show <commit> ──${R}"
  P3=$(run_cmd "p3" "$OUTDIR/p3.txt" \
    "$PIXEL_BIN" dig-history --show "$KIMI_COMMIT" . 2>/dev/null)
  head -20 "$OUTDIR/p3.txt" | sed 's/^/  /'
  echo -e "  ${DIM}... ($(wc -l < "$OUTDIR/p3.txt") lines total)${R}"
  echo -e "  ${DIM}Time: ${P3}ms${R}"
else
  M3=0; P3=0
  echo -e "  ${DIM}No commit found — skipping${R}"
fi

# ════════════════════════════════════════════════════════════════════════
# TASK 4: Find what files were affected by the Kimi feature across all commits
# ════════════════════════════════════════════════════════════════════════
header "TASK 4: Find all files touched by Kimi-related commits"

echo -e "\n  ${YLW}${B}── MANUAL: git log --grep kimi --name-only --pretty=format: ──${R}"
M4=$(run_cmd "m4" "$OUTDIR/m4.txt" \
  bash -c "git log --grep='$PHRASE' --all --name-only --pretty=format: 2>/dev/null | sort -u | grep -v '^$' | head -30")
  head -20 "$OUTDIR/m4.txt" | sed 's/^/  /'
echo -e "  ${DIM}... ($(wc -l < "$OUTDIR/m4.txt") files total)${R}"
echo -e "  ${DIM}Time: ${M4}ms${R}"

echo -e "\n  ${GRN}${B}── PIXEL: pixel search-content kimi --context 3 ──${R}"
P4=$(run_cmd "p4" "$OUTDIR/p4.txt" \
  "$PIXEL_BIN" search-content "$PHRASE" . 2>/dev/null)
head -20 "$OUTDIR/p4.txt" | sed 's/^/  /'
echo -e "  ${DIM}... ($(wc -l < "$OUTDIR/p4.txt") lines total)${R}"
echo -e "  ${DIM}Time: ${P4}ms${R}"

# ════════════════════════════════════════════════════════════════════════
# SUMMARY
# ════════════════════════════════════════════════════════════════════════
echo ""
echo -e "${B}${CYA}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${R}"
echo -e "${B}${CYA}                      SUMMARY                                 ${R}"
echo -e "${B}${CYA}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${R}"
echo ""
printf "  %-40s ${DIM}%-10s${R}  %-10s\n" "Task" "Manual" "Pixel"
printf "  %-40s %-10s  %-10s\n" "──────────────────────────────────────" "──────────" "──────────"

print_row() {
  local task="$1" m="$2" p="$3"
  local delta=$((p - m))
  local sign color
  if [ "$delta" -lt 0 ]; then
    sign="▼"; color="$GRN"
  elif [ "$delta" -gt 0 ]; then
    sign="▲"; color="$RED"
  else
    sign="="; color="$DIM"
  fi
  printf "  %-40s %-10s  ${color}%-10s${R}\n" "$task" "${m}ms" "${p}ms ${sign}"
}

print_row "1. Find commits mentioning kimi" "$M1" "$P1"
print_row "2. Find Kimi wiring commit" "$M2" "$P2"
print_row "3. Show the diff" "$M3" "$P3"
print_row "4. Find all affected files" "$M4" "$P4"

TOTAL_M=$((M1 + M2 + M3 + M4))
TOTAL_P=$((P1 + P2 + P3 + P4))
echo ""
echo -e "  ${B}Total manual:${R}  ${TOTAL_M}ms"
echo -e "  ${B}Total pixel:${R}   ${TOTAL_P}ms"
DELTA=$((TOTAL_P - TOTAL_M))
if [ "$DELTA" -lt 0 ]; then
  echo -e "  ${GRN}${B}Pixel saved $((DELTA * -1))ms${R}"
else
  echo -e "  ${RED}${B}Pixel added ${DELTA}ms${R}"
fi
echo ""
echo -e "  ${DIM}Full outputs: $OUTDIR/{m,p}-*.txt${R}"
echo ""
