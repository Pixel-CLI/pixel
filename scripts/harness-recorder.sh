#!/usr/bin/env bash
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

# harness-recorder.sh — record a harness run (Claude Code, Codex) with
# asciinema so a pull request can show the run as a terminal video.
#
#   scripts/harness-recorder.sh --provider claude --scenario scope
#   scripts/harness-recorder.sh --provider codex --scenario rns --gif
#
# What it does:
#   1. builds the natural-prompt scenario (same four prompts as
#      scripts/pixel-demo.sh, plus `rns` = run the repo's harness smoke test),
#   2. runs the harness under `asciinema rec` in a fresh PTY,
#   3. pretty-prints the machine event stream live, so the recording is a
#      readable video instead of a wall of JSONL,
#   4. writes harness-<provider>-<scenario>.cast (asciicast v3), .txt (plain
#      transcript) and, with --gif, .gif (agg), plus meta.json.
#
# Reporting (--post PR):
#   posts one PR comment with the stats table and the transcript in a
#   <details> block, and attaches the .cast as a secret gist so reviewers can
#   replay it (`agg <url>` or the asciinema player). --upload instead uploads
#   to an asciinema server you are already authenticated against
#   (`asciinema auth`) and embeds that URL.
#
# Exit codes: 0 recorded, 1 harness ran (or recording) but produced no events,
# 2 argument/precondition error.

set -u

OUTDIR="${HARNESS_OUTDIR:-/tmp/pixel-harness-recording}"
PROVIDER=""
REPO=""
SCENARIO="${SCENARIO:-scope}"
MAKE_GIF=0
POST_PR=""
UPLOAD=0
PROMPT_FILE=""
IDLE_LIMIT="2.0"
VIDEO_MAX_SECONDS="${HARNESS_VIDEO_MAX_SECONDS:-15}"
# --interactive records the real TUI (colors, spinner) instead of headless
# mode. The TUI does not exit when the task ends, so the runner watches the
# cast file: no screen updates for HARNESS_INTERACTIVE_IDLE seconds (the
# spinner redraws while the agent works, so silence means done), or
# HARNESS_INTERACTIVE_MAX seconds hard cap, whichever first.
INTERACTIVE=0
PROVIDER_DEFAULT_KEYS=""
INTERACTIVE_IDLE="${HARNESS_INTERACTIVE_IDLE:-30}"
INTERACTIVE_MAX="${HARNESS_INTERACTIVE_MAX:-300}"

usage() {
    sed -n '/^# harness-recorder.sh/,/^# Exit codes/p' "$0" | sed 's/^# \{0,1\}//'
}
die() {
    echo "harness-recorder: $*" >&2
    exit 2
}

while [ $# -gt 0 ]; do
    case "$1" in
        --provider) PROVIDER="${2:-}"; shift 2 ;;
        --provider=*) PROVIDER="${1#*=}"; shift ;;
        --repo) REPO="${2:-}"; shift 2 ;;
        --repo=*) REPO="${1#*=}"; shift ;;
        --scenario) SCENARIO="${2:-}"; shift 2 ;;
        --scenario=*) SCENARIO="${1#*=}"; shift ;;
        --gif) MAKE_GIF=1; shift ;;
        --post) POST_PR="${2:-}"; shift 2 ;;
        --post=*) POST_PR="${1#*=}"; shift ;;
        --upload) UPLOAD=1; shift ;;
        --interactive) INTERACTIVE=1; shift ;;
        --video-max-seconds) VIDEO_MAX_SECONDS="${2:-}"; shift 2 ;;
        --video-max-seconds=*) VIDEO_MAX_SECONDS="${1#*=}"; shift ;;
        --prompt-file) PROMPT_FILE="${2:-}"; shift 2 ;;
        --prompt-file=*) PROMPT_FILE="${1#*=}"; shift ;;
        --out) OUTDIR="${2:-}"; shift 2 ;;
        --out=*) OUTDIR="${1#*=}"; shift ;;
        --idle-time-limit) IDLE_LIMIT="${2:-}"; shift 2 ;;
        --help | -h) usage; exit 0 ;;
        *) die "unknown flag: $1 (see --help)" ;;
    esac
done

[ -n "$PROVIDER" ] || die "--provider is required (claude|codex)"
[ -n "$REPO" ] || REPO="$(pwd)"
case "$PROVIDER" in
    claude | codex | agy | pi) ;;
    *) die "--provider must be claude, codex, agy or pi, got '$PROVIDER'" ;;
esac
REPO=$(cd "$REPO" 2>/dev/null && pwd) || die "repo not found: $REPO"
[ -d "$REPO/.git" ] || [ -f "$REPO/.git" ] || die "$REPO is not a git repository"
command -v asciinema >/dev/null 2>&1 || die "asciinema not found: brew install asciinema"
command -v python3 >/dev/null 2>&1 || die "python3 not found"
# agg is only needed at render time; the recording itself can happen on a
# machine without it (the .cast is rendered into a GIF elsewhere).
case "$SCENARIO" in
    locate | scope | sync | recover | rns) ;;
    *) die "unknown scenario: $SCENARIO (locate|scope|sync|recover|rns)" ;;
esac

if [ -d "$OUTDIR" ] || mkdir -p "$OUTDIR" 2>/dev/null; then
    :
else
    die "cannot create output dir: $OUTDIR (parent missing?)"
fi
OUTDIR=$(cd "$OUTDIR" && pwd) || die "cannot enter output dir: $OUTDIR"

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BASE="${PROVIDER}-${SCENARIO}"
CAST="$OUTDIR/harness-$BASE.cast"
TXT="$OUTDIR/harness-$BASE.txt"
GIF="$OUTDIR/harness-$BASE.gif"
LOG="$OUTDIR/$BASE.jsonl"

# ── Prompt ────────────────────────────────────────────────────────────────
if [ -z "$PROMPT_FILE" ]; then
    PROMPT_FILE="$OUTDIR/prompt-$BASE.txt"
fi
mkdir -p "$(dirname "$PROMPT_FILE")"
if [ -n "${HARNESS_PROMPT_FULL:-}" ]; then
    printf '%s\n' "$HARNESS_PROMPT_FULL" > "$PROMPT_FILE"
else
    case "$SCENARIO" in
        locate)
            cat > "$PROMPT_FILE" << 'PROMPT'
You are working in the repository REPO_PLACEHOLDER (a Rust CLI tool).

Find where GUARD_MATCHER is defined and show its full definition with surrounding context. Report the file path, line number, and the full definition.
PROMPT
            ;;
        scope)
            cat > "$PROMPT_FILE" << 'PROMPT'
You are working in the repository REPO_PLACEHOLDER (a Rust CLI tool).

I want to add a new agent tool called "foobar" to the guard matcher. Find ALL files that would need to be modified for this change. List every file and why it needs changes.
PROMPT
            ;;
        sync)
            cat > "$PROMPT_FILE" << 'PROMPT'
You are working in the repository REPO_PLACEHOLDER (a Rust CLI tool).

Sync this branch with origin/main. Report what happened.
PROMPT
            ;;
        recover)
            cat > "$PROMPT_FILE" << 'PROMPT'
You are working in the repository REPO_PLACEHOLDER (a Rust CLI tool).

Find the deleted function register_mcp_server that was removed from the codebase. Show the commit that removed it, the file it was in, and the full original implementation.
PROMPT
            ;;
        rns)
            cat > "$PROMPT_FILE" << 'PROMPT'
You are working in the repository REPO_PLACEHOLDER (a Punkt CLI for averting memory-first hold on repo code),
where the pixel CLI is installed. This is a retrieval smoke run (the "RNS" checklist).

Do the following in order, reporting each step as you go:
1. Find where the guard matcher decides to rewrite a grep command into a pixel command. Use pixel to locate it (pixel find-code or pixel search-content), NOT plain grep.
2. Show the impact of changing that guard decision: which call sites and tests reference it (pixel impact).
3. Find the last commit that touched that region in git history (pixel excavate or pixel changes).
4. Report how many pixel commands you used instead of raw grep/sed.
PROMPT
            ;;
    esac
    if [[ "$(uname -s)" == "Darwin" ]]; then
        sed -i '' "s|REPO_PLACEHOLDER|$REPO|g" "$PROMPT_FILE"
    else
        sed -i "s|REPO_PLACEHOLDER|$REPO|g" "$PROMPT_FILE"
    fi
fi
[ -s "$PROMPT_FILE" ] || die "prompt file is empty: $PROMPT_FILE"

# ── The harness command. Its stdout goes to the video; the machine format
#    also lands in $LOG (stdout for claude) for pixel-call counting. The
#    filter colors the stream the video shows (the transcript strips ANSI on
#    convert): green marks a pixel call, cyan any other tool, dim the noise.
FILTER='
import json, re, sys, time
GRN = "\033[32m"; CYA = "\033[36m"; DIM = "\033[2m"; RST = "\033[0m"
pixel_pat = re.compile(r"(^|[\s/;&|\"])pixel\s+(search|resolve|targets|reconcile|excavate|rescue|impact|uses|changes|context|symbol|inspect|history|publish|push|ship|branch|update|sync|diff|review|ask|recall|find-code|search-content|scope-task)")
calls = 0
tools = []
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        evt = json.loads(line)
    except json.JSONDecodeError:
        print(DIM + line[:200] + RST); continue
    t = evt.get("type")
    if t == "assistant":
        msg = evt.get("message") or {}
        for block in (msg.get("content") or []):
            if isinstance(block, dict) and block.get("type") == "tool_use":
                inp = json.dumps(block.get("input", {}))
                is_pixel = bool(pixel_pat.search(inp))
                if is_pixel:
                    calls += 1
                tools.append(block.get("name", "?"))
                col = GRN if is_pixel else CYA
                print("%s  \u25b8 %s %s%s" % (col, block.get("name", "?"), inp[:120].replace("\n", " "), RST))
    elif t == "result":
        dur = evt.get("duration_ms"); txt = str(evt.get("result", ""))[:160].replace("\n", " ")
        print("%s  \u2713 result (%sms): %s%s" % (GRN, dur, txt, RST))
        seen = {}
        for name in tools:
            seen[name] = seen.get(name, 0) + 1
        summary = ", ".join("%s×%d" % (k, v) for k, v in sorted(seen.items(), key=lambda kv: -kv[1]))
        with open(sys.argv[1] if len(sys.argv) > 1 else "/dev/null", "w") as f:
            f.write("%d\t%s" % (calls, summary or "none"))
'

# ── Harness command, shell-quoted token by token (the generated runner script
#    replays this string; %q keeps paths and prompts safe).
q() { printf '%q ' "$@"; }
PROMPT_TEXT="$(cat "$PROMPT_FILE")"
case "$PROVIDER" in
    claude)
        # The user's real settings (pixel hooks installed) the way
        # pixel-demo.sh builds its pixel arm: deployed agent prompt + subagent
        # prompt, --verbose so stream-json emits tool events. Headless (-p)
        # unless --interactive, which runs the real TUI.
        AGENT_PROMPT="${AGENT_PROMPT:-$HOME/.local/share/pixel/agent-prompt.md}"
        [ -s "$AGENT_PROMPT" ] || AGENT_PROMPT="$ROOT/crates/pixel-install/assets/pixel-agent-prompt.md"
        SUBAGENT_PROMPT="${SUBAGENT_PROMPT:-$HOME/.local/share/pixel/subagent-prompt.md}"
        [ -s "$SUBAGENT_PROMPT" ] || SUBAGENT_PROMPT="$ROOT/crates/pixel-install/assets/pixel-subagent-prompt.md"
        [ -s "$AGENT_PROMPT" ] || die "agent prompt missing (run: pixel install)"
        command -v claude >/dev/null 2>&1 || die "claude not on PATH"
        if [ "$INTERACTIVE" -eq 1 ]; then
            RUNNER_CMD="$(q claude --dangerously-skip-permissions \
                --append-system-prompt-file "$AGENT_PROMPT" \
                --append-subagent-system-prompt-file "$SUBAGENT_PROMPT" \
                "$PROMPT_TEXT")"
        else
            RUNNER_CMD="$(q claude -p --dangerously-skip-permissions \
                --output-format stream-json --verbose \
                --append-system-prompt-file "$AGENT_PROMPT" \
                --append-subagent-system-prompt-file "$SUBAGENT_PROMPT") < $(printf '%q' "$PROMPT_FILE")"
        fi
        ;;
    codex)
        command -v codex >/dev/null 2>&1 || die "codex not on PATH"
        if [ "$INTERACTIVE" -eq 1 ]; then
            RUNNER_CMD="$(q codex --dangerously-bypass-approvals-and-sandbox \
                "$PROMPT_TEXT")"
            PROVIDER_DEFAULT_KEYS="sleep:8|n|sleep:2|text:$PROMPT_TEXT|Enter"
        else
            RUNNER_CMD="$(q codex exec --dangerously-bypass-approvals-and-sandbox \
                --skip-git-repo-check -C "$REPO" "$PROMPT_TEXT")"
        fi
        ;;
    agy)
        # macOS PATH can carry an Antigravity app-open shim first; the CLI
        # lives in ~/.local/bin (as in the OrbStack VM). AGY_BIN overrides.
        AGY_BIN="${AGY_BIN:-$HOME/.local/bin/agy}"
        [ -x "$AGY_BIN" ] || AGY_BIN="$(command -v agy 2>/dev/null)" || true
        [ -n "$AGY_BIN" ] && [ -x "$AGY_BIN" ] || die "agy not on PATH"
        # agy (Antigravity) has no system-prompt override flag: pixel reaches
        # it through pixel install's own wiring (AGENTS.md / hooks), which the
        # VM already has.
        if [ "$INTERACTIVE" -eq 1 ]; then
            # -i consumes the next token as the prompt: the permission flag
            # must come after it
            RUNNER_CMD="$(q "$AGY_BIN" -i="$PROMPT_TEXT" --dangerously-skip-permissions)"
            PROVIDER_DEFAULT_KEYS="Enter"
        else
            RUNNER_CMD="$(q "$AGY_BIN" -p --dangerously-skip-permissions \
                "$PROMPT_TEXT")"
        fi
        ;;
    pi)
        command -v pi >/dev/null 2>&1 || die "pi not on PATH"
        if [ "$INTERACTIVE" -eq 1 ]; then
            RUNNER_CMD="$(q pi "$PROMPT_TEXT")"
            PROVIDER_DEFAULT_KEYS=""
        else
            RUNNER_CMD="$(q pi --no-session "$PROMPT_TEXT")"
        fi
        ;;
esac

if [ "$INTERACTIVE" -eq 0 ]; then
# The recorded command is a generated script (not an inline bash -c string):
# the raw harness stream is teed to $LOG for counting, piped through the
# pretty-printer the video shows, and stderr is mirrored to a file without
# polluting the machine stream the filter has to parse.
RUNNER_SCRIPT="$OUTDIR/runner-$BASE.sh"
{
    printf '#!/usr/bin/env bash\n'
    printf '# generated by scripts/harness-recorder.sh — the recorded harness command\n'
    printf 'set -u\n'
    printf 'stty rows %d cols %d 2> /dev/null || true\n' "${AGG_ROWS:-36}" "${AGG_COLS:-112}"
    # stderr goes to a file only: the harness's own warnings (auth sources,
    # unknown model notices) would otherwise clutter the video; the mirror
    # file keeps them for debugging and pixel-call fallback counting.
    printf 'exec 2> %q\n' "$OUTDIR/$BASE.stderr.log"
    printf '%s | tee %q | python3 -c %q %q\n' \
        "$RUNNER_CMD" "$LOG" "$FILTER" "$OUTDIR/$BASE.counts"
    printf 'exit "${PIPESTATUS[0]}"\n'
} > "$RUNNER_SCRIPT"
else
# Interactive: the TUI renders itself (no filter). Default path wraps the
# session in a tmux server so first-launch consent dialogs can be answered
# with injected keys; HARNESS_INTERACTIVE_TMUX=0 runs the TUI directly under
# the recorder's PTY with idle-watch + SIGINT (agy refuses tmux panes).
RUNNER_SCRIPT="$OUTDIR/runner-$BASE.sh"
SESSION="shoot-$BASE"
if [ "${HARNESS_INTERACTIVE_TMUX:-1}" = "1" ] && command -v tmux >/dev/null 2>&1; then
{
    printf '#!/usr/bin/env bash\n'
    printf '# generated by scripts/harness-recorder.sh — the interactive harness session\n'
    printf 'set -u\n'
    printf 'stty rows %d cols %d 2> /dev/null || true\n' "${AGG_ROWS:-36}" "${AGG_COLS:-112}"
    printf 'TMUX_BIN=%q\n' "$(command -v tmux || echo tmux)"
    printf '"$TMUX_BIN" kill-session -t %q 2>/dev/null\n' "$SESSION"
    printf '"$TMUX_BIN" new-session -d -s %q -x %d -y %d %q\n' \
        "$SESSION" "${AGG_COLS:-112}" "${AGG_ROWS:-36}" \
        "cd $(printf '%q' "$REPO") && $RUNNER_CMD"
    # HARNESS_START_KEYS: |-separated tokens sent into the tmux pane after the
    # session starts, to answer consent dialogs and (for codex's task center)
    # type the prompt. Tokens: "sleep:N" waits, "text:..." types literally
    # (send-keys -l), anything else is a tmux key name. Defaults per provider:
    # claude Down|Enter (bypass warning defaults to No, exit), agy Enter
    # (trust dialog defaults to Yes), codex n|sleep:2|text:<prompt>|Enter,
    # pi nothing (prompt rides argv).
    printf 'START_KEYS=%q\n' "${HARNESS_START_KEYS:-$PROVIDER_DEFAULT_KEYS}"
    printf '( sleep 4; printf %s "$START_KEYS" | tr "|" "\\n" | while IFS= read -r k; do\n' '"$START_KEYS"'
    printf '    case "$k" in\n'
    printf '      sleep:*) sleep "${k#sleep:}" ;;\n'
    printf '      text:*) "$TMUX_BIN" send-keys -t %q -l "${k#text:}" 2>/dev/null ;;\n' "$SESSION"
    printf '      "") : ;;\n'
    printf '      *) "$TMUX_BIN" send-keys -t %q "$k" 2>/dev/null ;;\n' "$SESSION"
    printf '    esac\n'
    printf '  done ) &\n'
    printf '( start=$SECONDS; idle_start=""; prev_size=0; while :; do\n'
    printf '    sleep 2\n'
    printf '    size=$(wc -c < %q 2>/dev/null | tr -d " ")\n' "$CAST"
    printf '    if [ "${size:-0}" -gt "${prev_size:-0}" ]; then prev_size=$size; idle_start=$SECONDS; fi\n'
    printf '    if [ -n "${idle_start:-}" ] && [ $((SECONDS - idle_start)) -ge %d ] \\\n' "$INTERACTIVE_IDLE"
    printf '        && [ "${prev_size:-0}" -gt 2000 ]; then break; fi\n'
    printf '    if [ $((SECONDS - start)) -ge %d ]; then break; fi\n' "$INTERACTIVE_MAX"
    printf '  done; "$TMUX_BIN" kill-session -t %q 2>/dev/null\n' "$SESSION"
    printf ') &\n'
    printf 'exec "$TMUX_BIN" attach -t %q\n' "$SESSION"
} > "$RUNNER_SCRIPT"
else
{
    printf '#!/usr/bin/env bash\n'
    printf '# generated by scripts/harness-recorder.sh — the interactive harness session\n'
    printf 'set -u\n'
    printf 'stty rows %d cols %d 2> /dev/null || true\n' "${AGG_ROWS:-36}" "${AGG_COLS:-112}"
    printf 'cd %q\n' "$REPO"
    printf '%s &\n' "$RUNNER_CMD"
    printf 'pid=$!\n'
    printf 'start=$SECONDS; idle_start=""; prev_size=0\n'
    printf 'while kill -0 "$pid" 2>/dev/null; do\n'
    printf '  sleep 2\n'
    printf '  size=$(wc -c < %q 2>/dev/null | tr -d " ")\n' "$CAST"
    printf '  if [ "${size:-0}" -gt "$prev_size" ]; then prev_size=$size; idle_start=$SECONDS; fi\n'
    printf '  if [ $((SECONDS - start)) -ge %d ]; then break; fi\n' "$INTERACTIVE_MAX"
    printf '  if [ -n "$idle_start" ] && [ $((SECONDS - idle_start)) -ge %d ] && [ "$prev_size" -gt 2000 ]; then\n' "$INTERACTIVE_IDLE"
    printf '    kill -INT "$pid" 2>/dev/null\n'
    printf '    for w in 1 2 3 4 5; do kill -0 "$pid" 2>/dev/null || break; sleep 1; done\n'
    printf '    break\n'
    printf '  fi\n'
    printf 'done\n'
    printf 'kill -9 "$pid" 2>/dev/null\n'
    printf 'wait "$pid" 2>/dev/null\n'
    printf 'exit 0\n'
} > "$RUNNER_SCRIPT"
fi
fi
chmod +x "$RUNNER_SCRIPT"

echo "harness-recorder: provider=$PROVIDER scenario=$SCENARIO repo=$REPO" >&2
echo "  cast: $CAST  (transcript will be $TXT)" >&2
[ "$MAKE_GIF" -eq 1 ] && echo "  gif:  yes (agg)" >&2

START_MS=$(python3 -c 'import time; print(int(time.time()*1000))')

# ── Record. PTY is fresh (worse case: it inherits the caller's COLUMNS/
#    LINES, which is what makes a terminal-sized recording feel natural).
rm -f "$CAST"
asciinema rec \
    --command "bash \"$RUNNER_SCRIPT\"" \
    --output-format asciicast-v3 \
    --idle-time-limit "$IDLE_LIMIT" \
    --overwrite \
    --quiet \
    "$CAST" || true

[ -s "$CAST" ] || die "asciinema produced an empty recording: $CAST"

END_MS=$(python3 -c 'import time; print(int(time.time()*1000))')
WALL_MS=$((END_MS - START_MS))

# ── Plain transcript for the PR comment: colors off, control noise dropped.
# (asciinema 3's `cat` wants two or more files; `convert -f txt` is the
# single-file path.)
asciinema convert "$CAST" -f txt - 2>/dev/null | tr -d '\000' > "$TXT" ||
    { asciinema cat "$CAST" "$CAST" 2>/dev/null | tr -d '\000' > "$TXT"; }
[ -s "$TXT" ] || die "empty transcript from $CAST"

# ── Pixel-call count. claude's filter wrote "$BASE.counts"; codex's text log
#    is greppable directly; both fall back to the raw stream + stderr mirror.
COUNTS="$OUTDIR/$BASE.counts"
PIXEL_CALLS=0
TOOLS_USED="none"
if [ -f "$COUNTS" ]; then
    read -r PIXEL_CALLS TOOLS_USED < "$COUNTS"
fi
if [ "$PIXEL_CALLS" -eq 0 ]; then
    PIXEL_CALLS=$(grep -h -c -E '(^|[[:space:]/;&|"])pixel (search|find-code|search-content|resolve|targets|impact|changes|excavate|rescue|scope-task|inspect|recall)' \
        "$LOG" "$OUTDIR/$BASE.stderr.log" 2>/dev/null | awk -F: '{s+=$1} END {print s+0}')
    if [ "$PIXEL_CALLS" -eq 0 ] && [ -s "$CAST" ]; then
        # interactive TUIs have no machine stream: count pixel command echoes
        # across every event of the cast
        PIXEL_CALLS=$(python3 - "$CAST" << 'PY'
import json, re, sys
pat = re.compile(r"pixel\s+(search|find-code|search-content|resolve|targets|impact|changes|excavate|rescue|scope-task|inspect|recall|search-like-rg)")
n = 0
for line in open(sys.argv[1]):
    try:
        e = json.loads(line)
    except json.JSONDecodeError:
        continue
    if isinstance(e, list) and len(e) > 2 and e[1] == "o" and pat.search(e[2]):
        n += 1
print(n)
PY
)
    fi
    TOOLS_USED=$(grep -h -oE '"name":"[A-Za-z]+|pixel [a-z-]+' "$LOG" "$OUTDIR/$BASE.stderr.log" 2>/dev/null | sed 's/"name":"//' | sort | uniq -c | sort -rn | awk '{printf "%s×%s, ", $2, $1}' | sed 's/, $//')
    [ -n "$TOOLS_USED" ] || TOOLS_USED="none"
fi

# ── meta.json
python3 - "$OUTDIR/meta-$BASE.json" \
    "$PROVIDER" "$SCENARIO" "$REPO" "$WALL_MS" "$PIXEL_CALLS" \
    "$(python3 -c 'import time; print(time.strftime("%Y-%m-%dT%H:%M:%S%z"))')" << 'PY'
import json, sys
path = sys.argv[1]
fields = sys.argv[2:8]
meta = {
    "provider": fields[0],
    "scenario": fields[1],
    "repo": fields[2],
    "wall_ms": int(fields[3]),
    "pixel_calls": int(fields[4]),
    "recorded_at": fields[5],
}
with open(path, "w") as f:
    json.dump(meta, f, indent=2)
PY

if [ "$MAKE_GIF" -eq 1 ]; then
    # Playback speed scales so the GIF lands under VIDEO_MAX_SECONDS: the run
    # itself keeps its real duration (cutting an agent task short truncates
    # the work), but the video does not have to.
    CAST_SECONDS=$(python3 - "$CAST" << 'PY'
import json, sys
last = 0.0
for line in open(sys.argv[1]):
    try:
        evt = json.loads(line)
    except json.JSONDecodeError:
        continue
    if isinstance(evt, list) and len(evt) > 2 and evt[1] == "o" \
            and isinstance(evt[0], (int, float)):
        last = max(last, evt[0])
print(max(1, round(last)))
PY
)
    SPEED=1
    if [ "$CAST_SECONDS" -gt "$VIDEO_MAX_SECONDS" ]; then
        SPEED=$(( (CAST_SECONDS + VIDEO_MAX_SECONDS - 1) / VIDEO_MAX_SECONDS ))
    fi
    agg "$CAST" "$GIF" \
        --speed "$SPEED" \
        --font-size 14 \
        --theme asciinema \
        --cols "${AGG_COLS:-100}" --rows "${AGG_ROWS:-30}" \
        || die "agg failed on $CAST"
fi

echo "harness-recorder: done in ${WALL_MS}ms ($PIXEL_CALLS pixel calls)" >&2
echo "  cast: $CAST" >&2
echo "  txt:  $TXT" >&2
[ "$MAKE_GIF" -eq 1 ] && echo "  gif:  $GIF" >&2

# ── Report ────────────────────────────────────────────────────────────────
body_file="$OUTDIR/comment-$BASE.md"
{
    echo "## 🎥 Harness recording — $PROVIDER / $SCENARIO"
    echo
    echo "| | |"
    echo "| --- | --- |"
    echo "| Provider | \`$PROVIDER\` |"
    echo "| Scenario | \`$SCENARIO\` |"
    echo "| Wall clock | ${WALL_MS}ms |"
    echo "| Pixel invocations | $PIXEL_CALLS |"
    echo "| Tools used | $TOOLS_USED |"
    echo
    if [ "$UPLOAD" -eq 1 ]; then
        if URL=$(asciinema upload "$CAST" 2>/dev/null | grep -oE 'https://[^[:space:]]+'); then
            echo "**Watch:** re-watching at $URL — asciinema player embeds directly in GitHub comments:"
            echo "<video src='$URL' style='display:none'></video>"
        fi
    fi
    if [ -n "$POST_PR" ]; then
        echo "(this post is generated by scripts/harness-recorder.sh; replay with \`agg harness-*.cast\` after downloading the gist)"
    fi
    echo
    echo "<details><summary>Full transcript</summary>"
    echo
    echo '```'
    cat "$TXT"
    echo '```'
    echo
    echo "</details>"
} > "$body_file"

# media_branch_raw_url <gif> <branch-path> — push the GIF to the
# harness-recordings-media branch with git plumbing (no working-tree churn)
# and print its raw URL; empty on any failure (the comment then just lacks
# the inline video). gists are text-only: `gh gist create` refuses binaries,
# which is why the GIF needs a branch.
media_branch_raw_url() {
    git -C "$REPO" rev-parse --git-dir >/dev/null 2>&1 || return 0
    git -C "$REPO" ls-remote --exit-code origin refs/heads/harness-recordings-media >/dev/null 2>&1
    remote_url=$(git -C "$REPO" config --get remote.origin.url 2>/dev/null) || return 0
    case "$remote_url" in
        git@github.com:*) owner_repo=${remote_url#git@github.com:} ;;
        https://github.com/*) owner_repo=${remote_url#https://github.com/} ;;
        ssh://git@github.com/*) owner_repo=${remote_url#ssh://git@github.com/} ;;
        *) return 0 ;;
    esac
    owner_repo=${owner_repo%.git}
    local blob tree commit base parent branch_path="$2"
    blob=$(git -C "$REPO" hash-object -w "$1") || return 0
    git -C "$REPO" fetch -q origin harness-recordings-media 2>/dev/null
    if base=$(git -C "$REPO" rev-parse -q --verify FETCH_HEAD 2>/dev/null); then
        tree_base=$(git -C "$REPO" rev-parse -q --verify "$base^{tree}") || return 0
        parent=(-p "$base")
    else
        tree_base=""
        parent=()
    fi
    export GIT_INDEX_FILE="$OUTDIR/media-index"
    git -C "$REPO" read-tree ${tree_base:---empty}
    git -C "$REPO" update-index --add --cacheinfo "100644,$blob,$branch_path" || {
        unset GIT_INDEX_FILE
        return 0
    }
    tree=$(git -C "$REPO" write-tree)
    unset GIT_INDEX_FILE
    if [ ${#parent[@]} -gt 0 ]; then
        commit=$(git -C "$REPO" -c user.email=pixel-recorder@local -c user.name=pixel-recorder \
            commit-tree "$tree" -p "$base" -m "harness recording: $branch_path")
    else
        commit=$(git -C "$REPO" -c user.email=pixel-recorder@local -c user.name=pixel-recorder \
            commit-tree "$tree" -m "harness recording: $branch_path")
    fi
    git -C "$REPO" push -q origin "$commit:refs/heads/harness-recordings-media" 2>/dev/null || return 0
    printf 'https://raw.githubusercontent.com/%s/harness-recordings-media/%s\n' "$owner_repo" "$branch_path"
}

if [ -n "$POST_PR" ]; then
    command -v gh >/dev/null 2>&1 || die "--post: gh not on PATH"
    # The gist carries the .cast for offline replay (gh gists are text-only,
    # so the .gif cannot ride along); the GIF goes to the media branch and
    # its raw URL renders the video inline in the PR comment.
    GIST_URL=$(gh gist create "$CAST" -d "$PROVIDER/$SCENARIO" 2>/dev/null | tail -1) || true
    if [ "$UPLOAD" -eq 1 ]; then
        if UPLOAD_URL=$(asciinema upload "$CAST" 2>/dev/null | grep -oE 'https://[^[:space:]]+'); then
            {
                echo
                echo "**Watch:** $UPLOAD_URL"
            } >> "$body_file"
        fi
    fi
    if [ -n "$GIST_URL" ]; then
        {
            echo
            echo "**Replay offline:** $GIST_URL — \`agg <cast-url>\`"
        } >> "$body_file"
    fi
    if [ -f "$GIF" ]; then
        if GIF_RAW=$(media_branch_raw_url "$GIF" "recordings/$BASE.gif"); then
            [ -n "$GIF_RAW" ] && printf '\n![recording](%s)\n' "$GIF_RAW" >> "$body_file"
        fi
    fi
    gh pr comment "$POST_PR" --body-file "$body_file"
    echo "harness-recorder: posted comment on PR #$POST_PR" >&2
else
    echo "harness-recorder: comment body at $body_file (use --post <pr> to publish)" >&2
fi

exit 0
