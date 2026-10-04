#!/bin/sh
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

# pixel smoke test — exercises the INSTALLED pixel end to end: CLI surface,
# the guard hook's advisory contract across agent tool names, session-start,
# doctor, the install surface, the help of the mandatory workflows, and the
# pre-rename command names (accepted as aliases until 1.0).
#
#   scripts/pixel-smoke-test.sh                 # binary from `command -v pixel`
#   PIXEL_BIN=target/dev-release/pixel scripts/pixel-smoke-test.sh
#   PIXEL_SHELL=fish scripts/pixel-smoke-test.sh # doctor --shell when the
#                                               # login shell is not the one
#                                               # `claude` is launched from
#
# Read-only: nothing under $HOME is written. Run `pixel install` first; the
# doctor section reports what it left non-green. Exit 1 on any failure.
#
# The guard hook never blocks (see crates/pixel/src/guard.rs): destructive or
# substitutable git commands get an ADVISORY (exit 0, JSON note with a pixel
# alternative), a grep/rg on one file gets a transparent REWRITE (exit 0,
# `updatedInput` pointing at `pixel search-like-rg`), and everything else
# passes through silently. Those three shapes are what this test asserts.
set -u

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
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
# guard <payload> -> sets OUT and CODE
guard() {
    OUT=$(printf '%s' "$1" | "$PIXEL" run-hook guard 2>/dev/null); CODE=$?
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
expect_advisory() { # label needle
    ctx=$(json_field "$OUT" "d['hookSpecificOutput']['additionalContext']")
    if [ "$CODE" -eq 0 ] && printf '%s' "$ctx" | grep -q -- "$2"; then ok "$1: advisory names \`$2\`"
    else no "$1" "expected exit 0 + advisory containing \`$2\`, got exit $CODE: $(printf '%s' "$OUT" | head -c 200)"; fi
}
expect_rewrite() { # label
    cmd=$(json_field "$OUT" "d['hookSpecificOutput']['updatedInput']['command']")
    case "$cmd" in
        "pixel search-like-rg "*) [ "$CODE" -eq 0 ] && ok "$1: rewritten to \`pixel search-like-rg\`" || no "$1" "exit $CODE" ;;
        *) no "$1" "expected updatedInput.command = pixel search-like-rg …, got exit $CODE: $(printf '%s' "$OUT" | head -c 200)" ;;
    esac
}
expect_silent() { # label
    if [ "$CODE" -eq 0 ] && [ -z "$OUT" ]; then ok "$1: passthrough (exit 0, no output)"
    else no "$1" "expected exit 0 + empty output, got exit $CODE: $(printf '%s' "$OUT" | head -c 200)"; fi
}
expect_proceeds() { # label — exit 0, and if anything was printed it is JSON
    if [ "$CODE" -ne 0 ]; then no "$1" "exit $CODE"; return; fi
    if [ -z "$OUT" ] || [ -n "$(json_field "$OUT" "'json'")" ]; then ok "$1: proceeds (exit 0)"
    else no "$1" "non-JSON output: $(printf '%s' "$OUT" | head -c 200)"; fi
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

echo "=== 2. Guard hook — Claude tool names ==="
guard "$(payload Bash command "$RESET")";           expect_advisory "Claude Bash reset --hard" "pixel plan-rollback"
guard "$(payload Bash command "git commit -m x")";  expect_advisory "Claude Bash git commit" "pixel commit"
guard "$(payload Bash command "$GREP")";            expect_rewrite  "Claude Bash grep on one file"
guard "$(payload Read file_path "$SRC")";           expect_proceeds "Claude Read"
guard "$(payload Edit file_path "$SRC")";           expect_proceeds "Claude Edit"

echo "=== 3. Guard hook — Devin tool names ==="
guard "$(payload exec command "$RESET")";           expect_advisory "Devin exec reset --hard" "pixel plan-rollback"
guard "$(payload exec command "$GREP")";            expect_rewrite  "Devin exec grep on one file"
guard "$(payload read file_path "$SRC")";           expect_proceeds "Devin read"
guard "$(payload edit file_path "$SRC")";           expect_proceeds "Devin edit"
guard "$(payload find_file_by_name pattern "*.rs")"; expect_proceeds "Devin find_file_by_name"

echo "=== 3b. Guard hook — Codex tool names ==="
guard "$(payload bash command "$RESET")";           expect_advisory "Codex bash reset --hard" "pixel plan-rollback"
guard "$(payload apply_patch file_path "$SRC")";    expect_proceeds "Codex apply_patch"
guard "$(payload glob pattern "*.rs")";             expect_proceeds "Codex glob"
OUT=$(payload shell command "$GREP" | "$PIXEL" run-hook guard --provider codex 2>/dev/null); CODE=$?
expect_rewrite "Codex --provider codex shell grep on one file"

echo "=== 3c. Guard hook — Gemini tool names ==="
guard "$(payload run_shell_command command "$RESET")"; expect_advisory "Gemini run_shell_command reset --hard" "pixel plan-rollback"
guard "$(payload read_file file_path "$SRC")";      expect_proceeds "Gemini read_file"
guard "$(payload write_file file_path "$SRC")";     expect_proceeds "Gemini write_file"
guard "$(payload search pattern "test")";           expect_proceeds "Gemini search"

echo "=== 4. Guard hook — unknown tool name ==="
guard "$(payload webfetch url "https://example.invalid")"; expect_silent "unknown tool"

echo "=== 5. Guard hook — non-PreToolUse event ==="
guard "$(payload exec command "$RESET" PostToolUse)"; expect_silent "PostToolUse"

echo "=== 6. Guard hook — PIXEL_TARGETS_GUARD=0 override ==="
OUT=$(payload Bash command "$RESET" | PIXEL_TARGETS_GUARD=0 "$PIXEL" run-hook guard 2>/dev/null); CODE=$?
expect_silent "PIXEL_TARGETS_GUARD=0"

echo "=== 7. Session-start hook ==="
# `run-hook` is the current verb; `hook` is what every 0.2.x install wrote
# into agent settings, so both must answer.
for verb in run-hook hook; do
    OUT=$(printf '{}' | "$PIXEL" "$verb" session-start 2>/dev/null); CODE=$?
    [ "$CODE" -eq 0 ] && printf '%s' "$OUT" | grep -q capabilities && ok "$verb session-start emits the capability block" || no "$verb session-start" "exit $CODE"
done

echo "=== 8. Doctor ==="
# shellcheck disable=SC2086
DOC=$("$PIXEL" doctor "$REPO" --json $DOCTOR_SHELL 2>/dev/null)
printf '%s' "$DOC" | python3 -c '
import json,sys
d=json.load(sys.stdin)
s=d["summary"]; print("  green:",s["green"],"yellow:",s["yellow"],"red:",s["red"])
for c in d["checks"]:
    if c["status"]!="green": print("  ", c["status"].upper(), c["id"], "—", c["summary"])
sys.exit(0 if d["ok"] else 1)' && ok "doctor: ok" || no "doctor" "not ok (non-green checks listed above; PIXEL_SHELL=<shell> if only install.legacy-wrappers is red)"

echo "=== 9. Install surface (what \`pixel install\` deploys, read through doctor) ==="
for id in install.agent-prompt install.subagent-prompt install.pi-prompt install.legacy-wrappers install.codex-config rule.parity rule.scenarios; do
    st=$(json_field "$DOC" "next(c['status'] for c in d['checks'] if c['id']=='$id')")
    [ "$st" = green ] && ok "doctor $id green" || no "doctor $id" "status '${st:-missing}'"
done
[ -s "$HOME/.local/share/pixel/agent-prompt.md" ] && ok "agent-prompt.md deployed" || no "agent-prompt.md" "missing at ~/.local/share/pixel (run: pixel install)"

echo "=== 10. Mandatory workflows + release gate — help surface ==="
for cmd in scope-task find-code plan-rollback sync-branch check-release self-update; do
    "$PIXEL" "$cmd" --help 2>&1 | grep -F -q "Usage: $PIXEL_NAME $cmd" && ok "$cmd --help" || no "$cmd --help" "no usage line"
done
"$PIXEL" uninstall --help 2>&1 | grep -q -- "--wrappers-only" && ok "uninstall --wrappers-only documented" || no "uninstall --help" "no --wrappers-only"

echo "=== 11. Renamed commands — old and new names answer alike ==="
# old_and_new <old> <new> <args…>: both spellings exit 0 with one JSON
# document on stdout; only the old one prints the rename note on stderr.
# PIXEL_METRICS=1 overrides a PIXEL_METRICS=0 exported by the caller.
old_and_new() {
    old=$1; new=$2; shift 2
    ERR=$(mktemp)
    NEW_OUT=$(PIXEL_METRICS=1 "$PIXEL" --metrics on "$new" "$@" 2>"$ERR"); NEW_CODE=$?
    NEW_NOTE=$(grep -c "^note: '" "$ERR")
    OLD_OUT=$(PIXEL_METRICS=1 "$PIXEL" --metrics on "$old" "$@" 2>"$ERR"); OLD_CODE=$?
    OLD_NOTE=$(grep -c "^note: '$old' is now '$new'; the old name stays accepted until 1.0$" "$ERR")
    rm -f "$ERR"
    if [ "$NEW_CODE" -ne 0 ] || [ -z "$(json_field "$NEW_OUT" "'json'")" ]; then
        no "$new" "expected exit 0 + JSON, got exit $NEW_CODE: $(printf '%s' "$NEW_OUT" | head -c 200)"
    elif [ "$OLD_CODE" -ne 0 ] || [ -z "$(json_field "$OLD_OUT" "'json'")" ]; then
        no "$old (alias of $new)" "expected exit 0 + JSON, got exit $OLD_CODE: $(printf '%s' "$OLD_OUT" | head -c 200)"
    elif [ "$OLD_NOTE" -ne 1 ] || [ "$NEW_NOTE" -ne 0 ]; then
        no "$old (alias of $new)" "rename note: $OLD_NOTE line(s) for $old, $NEW_NOTE for $new (want 1 and 0)"
    else
        ok "$old and $new both answer with JSON; only $old prints the rename note"
    fi
}
old_and_new ready prepare-repo "$REPO" --no-daemon --json
old_and_new changes what-changed --json "$REPO"
old_and_new symbol find-symbol run_command --json "$REPO"
# `impact` was never renamed: one name, no note.
ERR=$(mktemp)
OUT=$(PIXEL_METRICS=1 "$PIXEL" --metrics on impact run_command --json "$REPO" 2>"$ERR"); CODE=$?
if [ "$CODE" -eq 0 ] && [ -n "$(json_field "$OUT" "'json'")" ] && ! grep -q "^note: '" "$ERR"; then ok "impact answers with JSON and no rename note"
else no "impact" "exit $CODE: $(printf '%s' "$OUT" | head -c 200)"; fi
rm -f "$ERR"
# Alias teaching is independent of metrics reporting.
ERR=$(mktemp)
PIXEL_METRICS=0 "$PIXEL" symbol run_command --json "$REPO" >/dev/null 2>"$ERR"
if grep -q "^note: 'symbol' is now 'find-symbol'; the old name stays accepted until 1.0$" "$ERR" && ! grep -q '🟩 Pixel' "$ERR"; then
    ok "PIXEL_METRICS=0 preserves rename teaching without metrics"
else
    no "PIXEL_METRICS=0" "expected rename note without metrics reporting"
fi
rm -f "$ERR"

echo ""
echo "=== RESULTS ==="
echo "PASS: $PASS  FAIL: $FAIL"
if [ "$FAIL" -eq 0 ]; then echo "ALL GREEN"; exit 0; else echo "HAS FAILURES"; exit 1; fi
