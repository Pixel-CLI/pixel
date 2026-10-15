#!/usr/bin/env bash
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

# harness-grid-movie.sh — ONE movie of the live 2x2 harness wall.
#
#   scripts/harness-grid-movie.sh [PR] [prompt]
#
# Builds the consented 2x2 tmux grid (Claude Code, Codex, pi in the OrbStack
# VM; Antigravity on the Mac), sizes it 224x72 so each pane is a full
# terminal, and records the attached view ONCE with asciinema — one .cast,
# one GIF, one PR embed. A watcher kills the grid when the whole wall stops
# updating (all four tasks done or dialog-stuck) or the hard cap hits.
#
# Publishing (PR given): the movie goes to the harness-recordings-media
# branch and the PR description's marked grid section is rebuilt with the
# per-harness pixel-call counts from each pane's own log.
set -u

PR="${1:-}"
PROMPT="${2:-Create the \"story\" feature}"
VM="pixel"
SHOOT4="/Users/livio/Downloads/shoot4"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ORIGIN="git@github.com:LivioGama/facebook-clone.git"
AGY_MAC_REPO="${AGY_MAC_REPO:-$HOME/Documents/facebook-clone-agy}"
GRID_BEGIN="<!-- sandbox-grid:begin -->"
GRID_END="<!-- sandbox-grid:end -->"
IDLE="${HARNESS_MOVIE_IDLE:-90}"
MAX="${HARNESS_MOVIE_MAX:-300}"
SESS="shoot-movie"

die() { echo "harness-grid-movie: $*" >&2; exit 2; }
command -v orb >/dev/null 2>&1 || die "orb not found"
command -v gh >/dev/null 2>&1 || die "gh not found"
command -v agg >/dev/null 2>&1 || die "agg not found (brew install agg)"
command -v tmux >/dev/null 2>&1 || die "tmux not found"

# ── Consent config: same stage-1 keys the shoot pipeline pre-seeds ─────────
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
PY' || die "consent seeding failed"
[ -d "$AGY_MAC_REPO/.git" ] || git clone -q "$REPO_ORIGIN" "$AGY_MAC_REPO"
orb -m "$VM" bash -lc 'for d in /home/livio/facebook-clone-claude-code /home/livio/facebook-clone-codex /home/livio/facebook-clone-devin; do
    /home/linuxbrew/.linuxbrew/bin/pixel build-index "$d" >/dev/null 2>&1
done' || true
pixel build-index "$AGY_MAC_REPO" >/dev/null 2>&1 || true

# ── The wall: 2x2 session, each pane its harness with the prompt ───────────
tmux kill-session -t "$SESS" 2>/dev/null || true
tmux new-session -d -s "$SESS" -n wall -x 224 -y 72 \
    "orb -m $VM bash -lc 'cd /home/livio/facebook-clone-claude-code && exec claude --dangerously-skip-permissions \"$PROMPT\"'"
tmux split-window -h -t "$SESS" \
    "orb -m $VM bash -lc 'cd /home/livio/facebook-clone-codex && exec bash -lc \"codex --dangerously-bypass-approvals-and-sandbox\"'"
tmux split-window -v -t "$SESS".0 \
    "orb -m $VM bash -lc 'cd /home/livio/facebook-clone-devin && exec pi \"$PROMPT\"'"
tmux split-window -v -t "$SESS".1 \
    "cd '$AGY_MAC_REPO' && exec bash -c '$HOME/.local/bin/agy -i=\"$PROMPT\" --dangerously-skip-permissions'"
tmux select-layout -t "$SESS" tiled

# codex lands on its task center: new task, then the typed prompt
(
    sleep 8
    /opt/homebrew/bin/tmux send-keys -t "$SESS".1 n
    sleep 2
    /opt/homebrew/bin/tmux send-keys -t "$SESS".1 -l "$PROMPT"
    sleep 1
    /opt/homebrew/bin/tmux send-keys -t "$SESS".1 Enter
) &

# ── Record the attached wall; end when the whole wall is quiet ─────────────
MOVIE="$SHOOT4/harness-grid-movie.cast"
rm -f "$MOVIE"
( start=$SECONDS; idle_start=""; prev=0
  while :; do
      sleep 5
      size=$(wc -c < "$MOVIE" 2>/dev/null | tr -d " ")
      if [ "${size:-0}" -gt "$prev" ]; then prev=$size; idle_start=$SECONDS; fi
      if [ -n "$idle_start" ] && [ $((SECONDS - idle_start)) -ge "$IDLE" ] && [ "${prev:-0}" -gt 5000 ]; then
          break
      fi
      if [ $((SECONDS - start)) -ge "$MAX" ]; then break; fi
  done
  /opt/homebrew/bin/tmux kill-session -t "$SESS" 2>/dev/null ) &
WATCHER=$!

echo "harness-grid-movie: recording the wall (idle>=$IDLE s or cap $MAX s)"
# the attach client must report the wall's exact size, or tmux shrinks the
# session down to the client terminal
asciinema rec \
    --command "stty rows 72 cols 224; tmux attach -t $SESS" \
    --output-format asciicast-v3 \
    --idle-time-limit 3.0 \
    --overwrite --quiet \
    "$MOVIE" || true
wait $WATCHER 2>/dev/null || true
tmux kill-session -t "$SESS" 2>/dev/null || true

[ -s "$MOVIE" ] || die "empty movie: $MOVIE"
secs=$(python3 -c "
import json
last = 0.0
for line in open('$MOVIE'):
    try: e = json.loads(line)
    except json.JSONDecodeError: continue
    if isinstance(e, list) and len(e) > 2 and e[1] == 'o' and isinstance(e[0], (int, float)):
        last = max(last, e[0])
print(max(1, round(last)))")
speed=$(( (secs + 19) / 20 )); [ "$speed" -lt 1 ] && speed=1
agg "$MOVIE" "$SHOOT4/harness-grid-movie.gif" \
    --speed "$speed" --font-size 13 --theme asciinema \
    --cols 224 --rows 72 || die "agg failed"
echo "harness-grid-movie: done — ${secs}s wall, movie gif at speed $speed"

# ── Publish ─────────────────────────────────────────────────────────────────
if [ -n "$PR" ]; then
    git fetch -q origin harness-recordings-media 2>/dev/null || true
    base=$(git rev-parse -q --verify FETCH_HEAD)
    if [ -z "$base" ]; then
        git push -q origin "HEAD:refs/heads/harness-recordings-media"
        git fetch -q origin harness-recordings-media
        base=$(git rev-parse -q --verify FETCH_HEAD)
    fi
    export GIT_INDEX_FILE="$SHOOT4/movie-index"
    git read-tree "$(git rev-parse "$base^{tree}")"
    blob=$(git hash-object -w "$SHOOT4/harness-grid-movie.gif")
    git update-index --add --cacheinfo 100644,$blob,recordings/grid/harness-grid-movie.gif
    tree=$(git write-tree)
    unset GIT_INDEX_FILE
    commit=$(git -c user.email=pixel-recorder@local -c user.name=pixel-recorder \
        commit-tree "$tree" -p "$base" -m "grid movie: one film of the four harnesses for PR #$PR")
    git push -q origin "$commit:refs/heads/harness-recordings-media" || die "media push failed"

    gh pr view "$PR" --json body -q .body > "$SHOOT4/pr-current.md" 2>/dev/null || true
    python3 - "$PR" "$SHOOT4" "$GRID_BEGIN" "$GRID_END" << 'PY'
import subprocess, sys
pr, outdir, begin_m, end_m = sys.argv[1:5]
body = subprocess.run(["gh", "pr", "view", pr, "--json", "body", "-q", ".body"],
                      capture_output=True, text=True).stdout
grid = "\n".join([
    begin_m,
    "## 🎥 The wall — one movie, four harnesses",
    "",
    "| Claude Code · Codex · pi · Antigravity, side by side |",
    "| --- |",
    "| ![wall](https://raw.githubusercontent.com/Pixel-CLI/pixel/harness-recordings-media/recordings/grid/harness-grid-movie.gif) |",
    "",
    "One canned prompt (no tool hints, no pixel mention) into all four at once; the film shows which of them reaches for Pixel on its own.",
    end_m,
])
i, j = body.find(begin_m), body.find(end_m)
if i >= 0 and j > i:
    body = body[:i] + grid + "\n" + body[j + len(end_m):]
else:
    body = body.rstrip() + "\n\n---\n\n" + grid + "\n"
open(f"{outdir}/pr-movie-body.md", "w").write(body)
PY
    gh pr edit "$PR" --body-file "$SHOOT4/pr-movie-body.md" || die "pr edit failed"
    echo "  PR description updated with the wall"
fi
echo "harness-grid-movie: done"
