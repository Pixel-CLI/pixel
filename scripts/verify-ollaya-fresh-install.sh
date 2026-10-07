#!/bin/sh
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

# verify-ollaya-fresh-install.sh — the reproducer for issue #407:
# "Verify Ollaya install + pull from a fresh Pixel environment".
#
# Runs the acceptance flow end to end in a contained environment:
#
#   clean Pixel environment → pixel install → ollaya setup → model pull
#
# Everything pixel and ollaya write lands under a throwaway HOME (with its
# own XDG dirs), so a run touches nothing outside it and can re-run on any
# machine with network access. Each step is checked as it completes; the
# script exits non-zero on the first failure and prints a PASS/FAIL line per
# step, so a regression is visible without reading the log.
#
# The ollaya setup is the classify-engine local install that `pixel install`
# offers at the end of an interactive global install and that
# `pixel config setup` runs directly: it downloads the ollaya installer,
# installs the `ollaya` binary into the pixel-managed prefix
# (~/.local/share/pixel/ollaya), pulls the recommended model (winnow:e4b,
# ~8 GB) and records the daemon launch that `pixel classify` auto-starts.
#
# Usage:
#   verify-ollaya-fresh-install.sh [path-to-pixel-binary]
#
# Environment:
#   PIXEL_BIN  pixel binary to test (default: `pixel` resolved on PATH)
#   KEEP=1     keep the throwaway HOME instead of deleting it on exit
#
# Requires: curl, zstd, script (util-linux), sha256sum, tar, awk, mktemp.
# The ollaya installer additionally needs glibc 2.38+ on Linux.

set -eu

PIXEL_BIN="${PIXEL_BIN:-pixel}"
if [ "$#" -ge 1 ]; then
    PIXEL_BIN="$1"
fi
KEEP="${KEEP:-0}"

for tool in curl zstd script sha256sum tar awk mktemp; do
    command -v "$tool" >/dev/null 2>&1 || {
        echo "error: required tool '$tool' not found on PATH" >&2
        exit 1
    }
done

command -v "$PIXEL_BIN" >/dev/null 2>&1 || {
    echo "error: pixel binary '$PIXEL_BIN' not found on PATH" >&2
    exit 1
}
"$PIXEL_BIN" --version >/dev/null 2>&1 || {
    echo "error: pixel binary '$PIXEL_BIN' does not run" >&2
    exit 1
}

# A clean Pixel environment: a fresh HOME and XDG dirs under one throwaway
# root, so nothing the run writes escapes and nothing pre-existing leaks in.
WORK="$(mktemp -d "${TMPDIR:-/tmp}/pixel-ollaya-fresh.XXXXXX")"
HOME_DIR="$WORK/home"
mkdir -p "$HOME_DIR"

cleanup() {
    if [ "$KEEP" = 1 ]; then
        echo "KEEP=1: throwaway environment left at $WORK" >&2
    else
        rm -rf "$WORK"
    fi
}
trap cleanup EXIT

export HOME="$HOME_DIR"
export XDG_CONFIG_HOME="$HOME_DIR/.config"
export XDG_DATA_HOME="$HOME_DIR/.local/share"
export XDG_STATE_HOME="$HOME_DIR/.local/state"
export XDG_CACHE_HOME="$HOME_DIR/.cache"
# Configure a private OLLAYA_HOST for the server this run owns, so model
# pulls target the managed store and not a pre-existing server.
export OLLAYA_HOST="127.0.0.1:11435"

# The pixel-managed ollaya prefix, matching OLLAYA_ROOT in
# crates/pixel/src/classify_setup.rs.
OLLAYA_ROOT="$XDG_DATA_HOME/pixel/ollaya"
OLLAYA_BIN="$OLLAYA_ROOT/bin/ollaya"
OLLAYA_MODELS="$OLLAYA_ROOT/models"
CONFIG_FILE="$HOME_DIR/.pixel/config.yaml"

pass() { echo "PASS: $1"; }
fail() {
    echo "FAIL: $1" >&2
    exit 1
}

echo "=== issue #407: fresh Pixel environment → pixel install → ollaya setup → model pull ==="
echo "pixel:    $PIXEL_BIN ($("$PIXEL_BIN" --version 2>/dev/null | head -1))"
echo "home:     $HOME_DIR (contained)"
echo

# --- [1/5] pixel install -------------------------------------------------------
# The global install deploys the agent prompt, hooks and shell wrapper into
# the fresh HOME. Non-interactive: the classify setup it would offer at the
# end is driven separately in step 2, where its output can be checked.
echo "--- [1/5] pixel install ---"
"$PIXEL_BIN" install --shell sh >"$WORK/install.out" 2>"$WORK/install.err" \
    || fail "pixel install exited non-zero: $(tail -5 "$WORK/install.err")"
pass "pixel install"

# --- [2/5] ollaya setup -------------------------------------------------------
# `pixel config setup` is the guided global setup; choosing the local
# classify engine runs the ollaya auto-setup (binary install + model pull +
# recorded launch). The setup needs a terminal, so it runs under a PTY
# (`script`); the scripted answers are seven Enters (keep each default:
# metrics on, daemon auto-start, task context, task boundary, advisory
# policy, classify enabled, save), then 3 (skip web search) and 1 (local).
echo "--- [2/5] ollaya setup (pixel config setup → local) ---"
printf '\r\r\r\r\r\r\r31' | PIXEL_BIN="$PIXEL_BIN" script -qec 'export PIXEL_BIN="${PIXEL_BIN}"; "$PIXEL_BIN" config setup' /dev/null \
    >"$WORK/setup.out" 2>&1 \
    || fail "pixel config setup exited non-zero; tail: $(tail -10 "$WORK/setup.out")"
grep -q "ollaya setup" "$WORK/setup.out" \
    || fail "setup did not reach the ollaya step; tail: $(tail -10 "$WORK/setup.out")"
pass "ollaya setup"

# --- [3/5] ollaya binary ------------------------------------------------------
echo "--- [3/5] ollaya binary ---"
[ -x "$OLLAYA_BIN" ] || fail "ollaya binary missing at $OLLAYA_BIN"
"$OLLAYA_BIN" --version >/dev/null 2>&1 || fail "ollaya --version failed"
pass "ollaya binary: $OLLAYA_BIN ($("$OLLAYA_BIN" --version 2>/dev/null | head -1))"

# --- [4/5] model pull ----------------------------------------------------------
# The pull ran as step [2/2] of the ollaya setup; verify the model the
# setup pulled is the one the daemon will serve.
echo "--- [4/5] model pull (winnow:e4b) ---"
if "$OLLAYA_BIN" list 2>/dev/null | grep -q "winnow:e4b"; then
    pass "model pull: winnow:e4b (ollaya list)"
elif [ -d "$OLLAYA_MODELS" ]; then
    # The ollaya list command may not show the model when no server is
    # running; fall back to scanning the model store's nested manifests
    # for the exact winnow:e4b model ID (issue #407).
    if find "$OLLAYA_MODELS" -type f -name "*.json" 2>/dev/null | head -20 | xargs grep -l "winnow:e4b" >/dev/null 2>&1; then
        pass "model pull: winnow:e4b (store manifest)"
    else
        fail "model winnow:e4b not found (ollaya list and store manifests empty or missing)"
    fi
else
    fail "model store $OLLAYA_MODELS missing — pull may not have run"
fi

# --- [5/5] config --------------------------------------------------------------
# The setup records the daemon launch and stores the engine preference;
# `pixel classify` reads both to auto-start the daemon on demand.
echo "--- [5/5] config ---"
[ -f "$CONFIG_FILE" ] || fail "global config missing at $CONFIG_FILE"
grep -q "engine: local" "$CONFIG_FILE" \
    || fail "classify engine not set to local in $CONFIG_FILE"
grep -q "ollaya" "$CONFIG_FILE" \
    || fail "ollaya launch not recorded in $CONFIG_FILE"
pass "config: engine=local, ollaya launch recorded"

echo
echo "ALL GREEN: fresh Pixel environment → pixel install → ollaya setup → model pull"
echo "Artifacts: $WORK (removed on exit; set KEEP=1 to inspect)"
