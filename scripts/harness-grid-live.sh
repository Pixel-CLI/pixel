#!/usr/bin/env bash
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

# harness-grid-live.sh — launch the real 2x2 grid of the four harnesses as a
# live CUI, no recording, for testing Pixel locally by hand.
#
#   scripts/harness-grid-live.sh [prompt]
#
# Panes: Claude Code (top-left), Codex (top-right), pi (bottom-left) all in
# their OrbStack VM repos; Antigravity (bottom-right) on the Mac, where its
# login lives. Everything is consented by config (folder trust, bypass mode,
# dangerous-mode prompt pre-seeded by scripts/harness-sandbox-shoot.sh's
# stage 1) — no pane should stop to ask. Detached: run once, then
# `tmux attach -t shoot-grid`. Kill it with `tmux kill-session -t shoot-grid`
# — the CLIs inside are real agent sessions and consume tokens while open.
set -u

PROMPT="${1:-Create the \"story\" feature}"
VM="pixel"
REPO="git@github.com:LivioGama/facebook-clone.git"
AGY_MAC_REPO="${AGY_MAC_REPO:-$HOME/Documents/facebook-clone-agy}"

command -v orb >/dev/null 2>&1 || { echo "harness-grid-live: orb not found" >&2; exit 2; }
command -v tmux >/dev/null 2>&1 || { echo "harness-grid-live: tmux not found" >&2; exit 2; }

# Consent config, same as the shoot pipeline's stage 1 (idempotent).
orb -m "$VM" bash -lc 'python3 - << "PY"
import json, os
repos = ["/home/livio/facebook-clone-claude-code",
         "/home/livio/facebook-clone-codex",
         "/home/livio/facebook-clone-devin"]
p = os.path.expanduser("~/.claude.json")
d = json.load(open(p)) if os.path.exists(p) and os.path.getsize(p) else {}
d["bypassPermissionsModeAccepted"] = True
d.setdefault("projects", {})
for repo in repos:
    proj = d["projects"].setdefault(repo, {})
    proj["bypassPermissionsModeAccepted"] = True
    proj["hasTrustDialogAccepted"] = True
json.dump(d, open(p, "w"))
sp = os.path.expanduser("~/.claude/settings.json")
s = json.load(open(sp)) if os.path.exists(sp) and os.path.getsize(sp) else {}
s["skipDangerousModePermissionPrompt"] = True
json.dump(s, open(sp, "w"))
PY' || exit 2
[ -d "$AGY_MAC_REPO/.git" ] || git clone -q "$REPO" "$AGY_MAC_REPO"

tmux kill-session -t shoot-grid 2>/dev/null || true
tmux new-session -d -s shoot-grid -n grid \
    "orb -m $VM bash -lc 'cd /home/livio/facebook-clone-claude-code && exec claude --dangerously-skip-permissions \"$PROMPT\"'"
tmux split-window -h -t shoot-grid \
    "orb -m $VM bash -lc 'cd /home/livio/facebook-clone-codex && exec bash -lc \"codex --dangerously-bypass-approvals-and-sandbox\"'"
tmux split-window -v -t shoot-grid.0 \
    "orb -m $VM bash -lc 'cd /home/livio/facebook-clone-devin && exec pi \"$PROMPT\"'"
tmux split-window -v -t shoot-grid.1 \
    "cd '$AGY_MAC_REPO' && exec bash -c '$HOME/.local/bin/agy -i=\"$PROMPT\" --dangerously-skip-permissions'"
tmux select-layout -t shoot-grid tiled
echo "harness-grid-live: grid running — attaching (detach with ctrl-b d, kill with tmux kill-session -t shoot-grid)"
exec tmux attach -t shoot-grid
