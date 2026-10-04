#!/usr/bin/env bash
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

# harness-sandbox-shoot.sh — one command: sandbox up, latest pixel, four
# harnesses shooting one natural prompt, four videos on the PR, live 2x2
# grid, auto-attached.
#
#   scripts/harness-sandbox-shoot.sh [PR] [prompt]
#   SKIP_SHOOT=1 scripts/harness-sandbox-shoot.sh 415   # re-render + republish only
#
# The sandbox is the OrbStack VM `pixel` (pluggable later: Rivet AgentOS,
# Docker Sandbox). Antigravity is shot on the Mac — its login lives in the
# macOS Keychain, which the VM cannot read — against a local clone of the
# same repo. One tmux server serves both sides (OrbStack shares /tmp).
#
# The prompt carries no tool hints and no mention of pixel: the video
# measures pixel discovery. Trust/consent dialogs are pre-accepted by config
# where a CLI has one (Claude's bypass warning); Antigravity's workspace
# trust dialog has no config form, so its start keys press Enter.

set -u

PR="${1:-}"
PROMPT="${2:-Create the \"story\" feature}"
VM="pixel"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
RECORDER="$SCRIPT_DIR/harness-recorder.sh"
OUTDIR="${HARNESS_OUTDIR:-$SCRIPT_DIR/shoot4}"
REPO_ORIGIN="git@github.com:LivioGama/facebook-clone.git"
AGY_MAC_REPO="${AGY_MAC_REPO:-$HOME/Documents/facebook-clone-agy}"
GRID_BEGIN="<!-- sandbox-grid:begin -->"
GRID_END="<!-- sandbox-grid:end -->"

die() { echo "harness-sandbox-shoot: $*" >&2; exit 2; }
trap 'rm -f "$OUTDIR/media-index"' EXIT
# Every shell these run through re-parses the prompt/repo: quote once here.
qprompt=$(printf '%q' "$PROMPT")
command -v orb >/dev/null 2>&1 || die "orb not found (OrbStack)"
command -v gh >/dev/null 2>&1 || die "gh not found"
command -v agg >/dev/null 2>&1 || die "agg not found (brew install agg)"
command -v tmux >/dev/null 2>&1 || die "tmux not found (brew install tmux)"

# ── 1. Sandbox up: VM running, tools in, pixel latest + install ────────────
orb start "$VM" 2>/dev/null || true
orb -m "$VM" bash -lc '
    command -v tmux >/dev/null 2>&1 || /home/linuxbrew/.linuxbrew/bin/brew install -q tmux
    command -v asciinema >/dev/null 2>&1 || {
        curl -sL -o ~/.local/bin/asciinema \
            https://github.com/asciinema/asciinema/releases/download/v3.2.1/asciinema-aarch64-unknown-linux-gnu
        chmod +x ~/.local/bin/asciinema
    }
    /home/linuxbrew/.linuxbrew/bin/brew upgrade pixel >/dev/null 2>&1 \
        || echo "  (keeping the installed pixel; the tap has no newer bottle yet)" >&2
    /home/linuxbrew/.linuxbrew/bin/pixel install >/dev/null 2>&1 || true
    python3 - << "PY"
import json, os
# pre-accept every consent Claude Code can raise: folder trust, bypass-mode
# warning, dangerous-mode prompt — for every shoot repo, so a session never
# stops to ask (the keys are the ones the manual acceptances wrote).
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
print("  claude consents pre-accepted for", len(repos), "repos")
PY
' || die "sandbox prep failed"
echo "harness-sandbox-shoot: sandbox ready ($(orb -m $VM bash -lc 'pixel --version' | head -1))"

# ── 2. Repos: one per harness, pre-indexed off camera ──────────────────────
orb -m "$VM" bash -lc 'for d in /home/livio/facebook-clone-claude-code /home/livio/facebook-clone-codex /home/livio/facebook-clone-devin; do
    /home/linuxbrew/.linuxbrew/bin/pixel build-index "$d" >/dev/null 2>&1
done' || true
if [ ! -d "$AGY_MAC_REPO/.git" ]; then
    git clone -q "$REPO_ORIGIN" "$AGY_MAC_REPO" || die "cannot clone the sandbox repo for agy"
fi
pixel build-index "$AGY_MAC_REPO" >/dev/null 2>&1 || true

# ── 3. Shoot: four recorders in parallel (SKIP_SHOOT=1 reuses casts) ───────
mkdir -p "$OUTDIR"
if [ -z "${SKIP_SHOOT:-}" ]; then
    qoutdir=$(printf '%q' "$OUTDIR")
    qrecorder=$(printf '%q' "$RECORDER")
    for spec in "claude|/home/livio/facebook-clone-claude-code|vm" \
                "codex|/home/livio/facebook-clone-codex|vm" \
                "pi|/home/livio/facebook-clone-devin|vm" \
                "agy|$AGY_MAC_REPO|mac"; do
        provider="${spec%%|*}"
        rest="${spec#*|}"
        repo="${rest%%|*}"
        host="${rest##*|}"
        qprovider=$(printf '%q' "$provider")
        qrepo=$(printf '%q' "$repo")
        if [ "$host" = "vm" ]; then
            orb -m "$VM" bash -lc "env IS_SANDBOX=1 \
                HARNESS_PROMPT_FULL=$qprompt HARNESS_OUTDIR=$qoutdir \
                HARNESS_INTERACTIVE_MAX=\"${HARNESS_INTERACTIVE_MAX:-300}\" HARNESS_INTERACTIVE_TMUX=1 \
                PATH=/home/linuxbrew/.linuxbrew/bin:/home/livio/.local/bin:\$PATH \
                bash $qrecorder \
                --provider $qprovider --repo $qrepo --scenario rns --interactive" \
                > "$OUTDIR/shoot-$provider.log" 2>&1 &
        else
            HARNESS_PROMPT_FULL="$PROMPT" HARNESS_OUTDIR="$OUTDIR" \
                HARNESS_INTERACTIVE_MAX="${HARNESS_INTERACTIVE_MAX:-300}" HARNESS_INTERACTIVE_IDLE="${HARNESS_INTERACTIVE_IDLE:-90}" \
                HARNESS_INTERACTIVE_TMUX=1 \
                bash "$RECORDER" \
                --provider "$provider" --repo "$repo" --scenario rns --interactive \
                > "$OUTDIR/shoot-$provider.log" 2>&1 &
        fi
    done
    echo "harness-sandbox-shoot: four shoots running (claude, codex, pi in the VM; agy on the Mac)"
    wait
    echo "harness-sandbox-shoot: all shoots landed"
fi

# ── 4. Videos: cast seconds → agg speed, ≤15 s each ────────────────────────
for provider in claude codex pi agy; do
    cast="$OUTDIR/harness-$provider-rns.cast"
    [ -s "$cast" ] || { echo "  missing cast: $provider" >&2; continue; }
    secs=$(python3 -c "
import json
last = 0.0
for line in open('$cast'):
    try: e = json.loads(line)
    except json.JSONDecodeError: continue
    if isinstance(e, list) and len(e) > 2 and e[1] == 'o' and isinstance(e[0], (int, float)):
        last = max(last, e[0])
print(max(1, round(last)))")
    speed=$(( (secs + 14) / 15 )); [ "$speed" -lt 1 ] && speed=1
    agg "$cast" "$OUTDIR/harness-$provider-rns.gif" \
        --speed "$speed" --font-size 14 --theme asciinema \
        --cols 112 --rows 36 || true
    echo "  $provider: ${secs}s cast → gif at speed $speed"
done

# ── 5. Live 2x2 grid tmux (detached; the script attaches at the end) ───────
tmux kill-session -t shoot-grid 2>/dev/null || true
tmux new-session -d -s shoot-grid -n grid \
    "orb -m $VM bash -lc $(printf '%q' "cd /home/livio/facebook-clone-claude-code && exec claude --dangerously-skip-permissions $qprompt")"
tmux split-window -h -t shoot-grid \
    "orb -m $VM bash -lc $(printf '%q' "cd /home/livio/facebook-clone-codex && exec codex $qprompt")"
tmux split-window -v -t shoot-grid.0 \
    "orb -m $VM bash -lc $(printf '%q' "cd /home/livio/facebook-clone-devin && exec pi $qprompt")"
tmux split-window -v -t shoot-grid.1 \
    "cd $(printf '%q' "$AGY_MAC_REPO") && exec bash -c $(printf '%q' "$HOME/.local/bin/agy -i=$qprompt --dangerously-skip-permissions")"
tmux select-layout -t shoot-grid tiled
echo "  live grid running"

# ── 6. Publish: GIFs to the media branch, counts into the PR description ──
if [ -n "$PR" ]; then
    git fetch -q origin harness-recordings-media 2>/dev/null || {
        git push -q origin "HEAD:refs/heads/harness-recordings-media"
        git fetch -q origin harness-recordings-media
    }
    base=$(git rev-parse -q --verify FETCH_HEAD)
    export GIT_INDEX_FILE="$OUTDIR/media-index"
    git read-tree "$(git rev-parse "$base^{tree}")" || die "media read-tree failed"
    for provider in claude codex pi agy; do
        gif="$OUTDIR/harness-$provider-rns.gif"
        [ -s "$gif" ] || continue
        blob=$(git hash-object -w "$gif") || die "media hash-object failed ($provider)"
        git update-index --add --cacheinfo "100644,$blob,recordings/grid/$provider-rns.gif" \
            || die "media update-index failed ($provider)"
    done
    tree=$(git write-tree) || die "media write-tree failed"
    unset GIT_INDEX_FILE
    rm -f "$OUTDIR/media-index"
    commit=$(git -c user.email=pixel-recorder@local -c user.name=pixel-recorder \
        commit-tree "$tree" -p "$base" -m "sandbox grid: story-feature shoots for PR #$PR")
    git push -q origin "$commit:refs/heads/harness-recordings-media" || die "media push failed"
    echo "  media branch updated"

    gh pr view "$PR" --json body -q .body > "$OUTDIR/pr-body-current.md" 2>/dev/null || true
    if ! python3 - "$PR" "$OUTDIR" "$GRID_BEGIN" "$GRID_END" << 'PY'
import json, subprocess, sys
pr, outdir, begin_m, end_m = sys.argv[1:5]
result = subprocess.run(["gh", "pr", "view", pr, "--json", "body", "-q", ".body"],
                        capture_output=True, text=True)
if result.returncode != 0:
    raise SystemExit("gh pr view failed")
body = result.stdout
rows = ""
for p in ("claude", "codex", "pi", "agy"):
    try:
        with open(f"{outdir}/meta-{p}-rns.json") as f:
            m = json.load(f)
    except FileNotFoundError:
        rows += f"| {p} | n/a | n/a |\n"
        continue
    rows += f"| {p} | {m['pixel_calls']} | {round(m['wall_ms'] / 1000)}s |\n"
grid = "\n".join([
    begin_m,
    "## 🎥 Pixel discovery — sandbox grid (one natural prompt, no tool hints)",
    "",
    "| harness | pixel calls | wall |",
    "| --- | --- | --- |",
    rows.rstrip(),
    "",
    "| Claude + pi | Codex + Antigravity |",
    "| --- | --- |",
    "| ![claude](https://raw.githubusercontent.com/Pixel-CLI/pixel/harness-recordings-media/recordings/grid/claude-rns.gif) ![pi](https://raw.githubusercontent.com/Pixel-CLI/pixel/harness-recordings-media/recordings/grid/pi-rns.gif) | ![codex](https://raw.githubusercontent.com/Pixel-CLI/pixel/harness-recordings-media/recordings/grid/codex-rns.gif) ![agy](https://raw.githubusercontent.com/Pixel-CLI/pixel/harness-recordings-media/recordings/grid/agy-rns.gif) |",
    end_m,
])
i, j = body.find(begin_m), body.find(end_m)
if i >= 0 and j > i:
    body = body[:i] + grid + "\n" + body[j + len(end_m):]
else:
    body = body.rstrip() + "\n\n---\n\n" + grid + "\n"
open(f"{outdir}/pr-body.md", "w").write(body)
PY
    then
        die "PR description retrieval failed"
    fi
    gh pr edit "$PR" --body-file "$OUTDIR/pr-body.md" || die "pr edit failed"
    echo "  PR description updated"
fi

echo "harness-sandbox-shoot: done — attaching the live grid"
exec tmux attach -t shoot-grid
