#!/bin/sh
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

# pixel smoke test — exercises the INSTALLED pixel end to end: CLI surface,
# the hook verbs an earlier release left registered, doctor, the install
# surface and the help of the mandatory workflows.
#
#   scripts/dev/pixel-smoke-test.sh                 # binary from `command -v pixel`
#   PIXEL_BIN=target/dev-release/pixel scripts/dev/pixel-smoke-test.sh
#   PIXEL_SHELL=fish scripts/dev/pixel-smoke-test.sh # doctor --shell when the
#                                               # login shell is not the one
#                                               # `claude` is launched from
#
# Read-only: nothing under $HOME is written. Run `pixel setup` first; the
# doctor section reports what it left non-green. Exit 1 on any failure.
#
# `pixel run-hook task-event` and `metrics` are the hooks `pixel setup`
# registers. A verb an earlier release registered (guard, composed-guard,
# session-start, prompt-submit, post-compaction, post-tool-use) is no longer
# parsed, but `run-hook` still exits 0 with nothing on stdout for it, so a
# registration an old install left behind can never block or fail a host;
# section 2 asserts exactly that.
set -u

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
PIXEL="${PIXEL_BIN:-$(command -v pixel 2>/dev/null || true)}"
if [ -z "$PIXEL" ]; then
    for p in "$ROOT/target/dev-release/pixel" "$ROOT/target/release/pixel"; do
        [ -x "$p" ] && PIXEL="$p" && break
    done
fi
if [ -z "$PIXEL" ] || [ ! -x "$PIXEL" ]; then
    echo "pixel-smoke-test: no pixel binary (PIXEL_BIN unset, none on PATH, none under target/)." >&2
    echo "  build + install one: pixel self-update --repo . --build \"cargo build --profile dev-release -p pixel-cli\"" >&2
    exit 2
fi
REPO="$ROOT"
PIXEL_NAME="${PIXEL##*/}"
DOCTOR_SHELL=""
[ -n "${PIXEL_SHELL:-}" ] && DOCTOR_SHELL="--shell $PIXEL_SHELL"

PASS=0; FAIL=0
ok() { echo "PASS: $1"; PASS=$((PASS+1)); }
no() { echo "FAIL: $1 — $2"; FAIL=$((FAIL+1)); }

# payload <tool_name> <input_key> <input_value> [event]
payload() {
    printf '{"hook_event_name":"%s","tool_name":"%s","cwd":"%s","tool_input":{"%s":"%s"}}' \
        "${4:-PreToolUse}" "$1" "$REPO" "$2" "$3"
}
# json_field <json> <python expression over d> -> prints the value or ""
json_field() {
    printf '%s' "$1" | python3 -c "
import json,sys
try:
    d=json.load(sys.stdin)
except Exception:
    print(''); sys.exit(0)
try:
    print($2)
except Exception:
    print('')"
}
expect_silent() { # label
    if [ "$CODE" -eq 0 ] && [ -z "$OUT" ]; then ok "$1: passthrough (exit 0, no output)"
    else no "$1" "expected exit 0 + empty output, got exit $CODE: $(printf '%s' "$OUT" | head -c 200)"; fi
}

SRC="$REPO/crates/pixel/src/main.rs"
RESET="git reset --hard HEAD~1"
GREP="grep -n login_user README.md"

echo "=== 0. Binary ==="
echo "  $PIXEL"
"$PIXEL" --version 2>/dev/null | sed 's/^/  /'

echo "=== 1. CLI surface ==="
"$PIXEL" --version 2>/dev/null | grep -q '^commit: ' && ok "--version reports commit/target/rustc/built" || no "--version" "no \`commit:\` line"
[ "$("$PIXEL" -V 2>/dev/null | wc -l | tr -d ' ')" = 1 ] && ok "-V is one line" || no "-V" "expected one line"
"$PIXEL" --help 2>&1 | grep -q "pixel" && ok "--help" || no "--help" "no output"

echo "=== 2. Hook verbs of earlier releases — fail open ==="
# An old install may still register any of these; each must exit 0 and print
# nothing on stdout whatever the payload says, so a stale registration cannot
# block a tool call, rewrite a command or inject context.
for verb in guard composed-guard session-start prompt-submit post-compaction post-tool-use; do
    OUT=$(payload Bash command "$RESET" | "$PIXEL" run-hook "$verb" --provider claude 2>/dev/null); CODE=$?
    expect_silent "run-hook $verb"
done
OUT=$(payload shell command "$GREP" | "$PIXEL" run-hook guard --provider codex 2>/dev/null); CODE=$?
expect_silent "run-hook guard --provider codex"

echo "=== 3. Doctor ==="
# shellcheck disable=SC2086
DOC=$("$PIXEL" doctor "$REPO" --json $DOCTOR_SHELL 2>/dev/null)
printf '%s' "$DOC" | python3 -c '
import json,sys
d=json.load(sys.stdin)
s=d["summary"]; print("  green:",s["green"],"yellow:",s["yellow"],"red:",s["red"])
for c in d["checks"]:
    if c["status"]!="green": print("  ", c["status"].upper(), c["id"], "—", c["summary"])
sys.exit(0 if d["ok"] else 1)' && ok "doctor: ok" || no "doctor" "not ok (non-green checks listed above; PIXEL_SHELL=<shell> if only binary.shell-path is red)"

echo "=== 4. Install surface (what \`pixel setup\` leaves, read through doctor) ==="
for id in install.pi-impact install.claude-hooks; do
    st=$(json_field "$DOC" "next(c['status'] for c in d['checks'] if c['id']=='$id')")
    [ "$st" = green ] && ok "doctor $id green" || no "doctor $id" "status '${st:-missing}'"
done
[ -s "$HOME/.claude/skills/pixel/SKILL.md" ] && ok "the pixel skill is deployed for Claude" || no "pixel skill" "no ~/.claude/skills/pixel/SKILL.md (run: pixel setup)"

echo "=== 5. Mandatory workflows + release gate — help surface ==="
for cmd in scope-task find-code sync-branch check-release self-update; do
    "$PIXEL" "$cmd" --help 2>&1 | grep -F -q "Usage: $PIXEL_NAME $cmd" && ok "$cmd --help" || no "$cmd --help" "no usage line"
done
"$PIXEL" setup --help 2>&1 | grep -q -- "--remove" && ok "setup --remove documented" || no "setup --help" "no --remove"

echo "=== 6. A core retrieval command answers with JSON ==="
OUT=$(PIXEL_METRICS=1 "$PIXEL" --metrics on impact run_command --json "$REPO" 2>/dev/null); CODE=$?
if [ "$CODE" -eq 0 ] && [ -n "$(json_field "$OUT" "'json'")" ]; then ok "impact answers with JSON"
else no "impact" "exit $CODE: $(printf '%s' "$OUT" | head -c 200)"; fi

echo ""
echo "=== RESULTS ==="
echo "PASS: $PASS  FAIL: $FAIL"
if [ "$FAIL" -eq 0 ]; then echo "ALL GREEN"; exit 0; else echo "HAS FAILURES"; exit 1; fi
