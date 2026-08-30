#!/bin/sh
# pixel smoke test — exercises guard hook with both Claude + Devin tool names,
# core CLI commands, and the four mandatory workflows.
PIXEL=~/.local/bin/pixel
REPO=/Users/livio/Documents/pixel
PASS=0; FAIL=0
ok() { echo "PASS: $1"; PASS=$((PASS+1)); }
no() { echo "FAIL: $1 — $2"; FAIL=$((FAIL+1)); }

echo "=== 1. CLI surface ==="
$PIXEL --version >/dev/null 2>&1 && ok "--version" || no "--version" "exit $?"
$PIXEL --help 2>&1 | grep -q "pixel" && ok "--help" || no "--help" "no output"

echo "=== 2. Guard hook — Claude tool names ==="
# Bash with git reset --hard in an indexed repo → should block (exit 2)
echo '{"hook_event_name":"PreToolUse","tool_name":"Bash","cwd":"'"$REPO"'","tool_input":{"command":"git reset --hard HEAD~1"}}' | $PIXEL hook guard 2>/dev/null
[ $? -eq 2 ] && ok "Claude Bash: git reset --hard blocked" || no "Claude Bash" "expected exit 2"

# Read on a file in indexed repo with no manifest → exit 0 (reads are not mandated)
echo '{"hook_event_name":"PreToolUse","tool_name":"Read","cwd":"'"$REPO"'","tool_input":{"file_path":"'"$REPO"'/crates/pixel/src/main.rs"}}' | $PIXEL hook guard 2>/dev/null
[ $? -eq 0 ] && ok "Claude Read: no block (reads not mandated without manifest)" || no "Claude Read" "expected exit 0"

# Edit on a non-exempt file in indexed repo with no manifest → should mandate (exit 2)
echo '{"hook_event_name":"PreToolUse","tool_name":"Edit","cwd":"'"$REPO"'","tool_input":{"file_path":"'"$REPO"'/crates/pixel/src/main.rs"}}' | $PIXEL hook guard 2>/dev/null
[ $? -eq 2 ] && ok "Claude Edit: mandate block (no manifest)" || no "Claude Edit" "expected exit 2"

echo "=== 3. Guard hook — Devin tool names ==="
# exec with git reset --hard → should block (exit 2)
echo '{"hook_event_name":"PreToolUse","tool_name":"exec","cwd":"'"$REPO"'","tool_input":{"command":"git reset --hard HEAD~1"}}' | $PIXEL hook guard 2>/dev/null
[ $? -eq 2 ] && ok "Devin exec: git reset --hard blocked" || no "Devin exec" "expected exit 2"

# read on a file in indexed repo with no manifest → exit 0 (reads not mandated)
echo '{"hook_event_name":"PreToolUse","tool_name":"read","cwd":"'"$REPO"'","tool_input":{"file_path":"'"$REPO"'/crates/pixel/src/main.rs"}}' | $PIXEL hook guard 2>/dev/null
[ $? -eq 0 ] && ok "Devin read: no block (reads not mandated without manifest)" || no "Devin read" "expected exit 0"

# edit on a non-exempt file in indexed repo with no manifest → should mandate (exit 2)
echo '{"hook_event_name":"PreToolUse","tool_name":"edit","cwd":"'"$REPO"'","tool_input":{"file_path":"'"$REPO"'/crates/pixel/src/main.rs"}}' | $PIXEL hook guard 2>/dev/null
[ $? -eq 2 ] && ok "Devin edit: mandate block (no manifest)" || no "Devin edit" "expected exit 2"

# grep tool name → should not crash (exit 0, no manifest check for read-only without manifest)
echo '{"hook_event_name":"PreToolUse","tool_name":"grep","cwd":"'"$REPO"'","tool_input":{"pattern":"test"}}' | $PIXEL hook guard 2>/dev/null
[ $? -eq 0 ] && ok "Devin grep: no crash (no manifest)" || no "Devin grep" "expected exit 0"

# find_file_by_name → should not crash
echo '{"hook_event_name":"PreToolUse","tool_name":"find_file_by_name","cwd":"'"$REPO"'","tool_input":{"pattern":"*.rs"}}' | $PIXEL hook guard 2>/dev/null
[ $? -eq 0 ] && ok "Devin find_file_by_name: no crash" || no "Devin find_file_by_name" "expected exit 0"

echo "=== 3b. Guard hook — Codex tool names ==="
# bash with git reset --hard → should block (exit 2)
echo '{"hook_event_name":"PreToolUse","tool_name":"bash","cwd":"'"$REPO"'","tool_input":{"command":"git reset --hard HEAD~1"}}' | $PIXEL hook guard 2>/dev/null
[ $? -eq 2 ] && ok "Codex bash: git reset --hard blocked" || no "Codex bash" "expected exit 2"

# apply_patch on a non-exempt file → should mandate (exit 2)
echo '{"hook_event_name":"PreToolUse","tool_name":"apply_patch","cwd":"'"$REPO"'","tool_input":{"file_path":"'"$REPO"'/crates/pixel/src/main.rs"}}' | $PIXEL hook guard 2>/dev/null
[ $? -eq 2 ] && ok "Codex apply_patch: mandate block (no manifest)" || no "Codex apply_patch" "expected exit 2"

# glob → should not crash
echo '{"hook_event_name":"PreToolUse","tool_name":"glob","cwd":"'"$REPO"'","tool_input":{"pattern":"*.rs"}}' | $PIXEL hook guard 2>/dev/null
[ $? -eq 0 ] && ok "Codex glob: no crash" || no "Codex glob" "expected exit 0"

echo "=== 3c. Guard hook — Gemini tool names ==="
# run_shell_command with git reset --hard → should block (exit 2)
echo '{"hook_event_name":"PreToolUse","tool_name":"run_shell_command","cwd":"'"$REPO"'","tool_input":{"command":"git reset --hard HEAD~1"}}' | $PIXEL hook guard 2>/dev/null
[ $? -eq 2 ] && ok "Gemini run_shell_command: git reset --hard blocked" || no "Gemini run_shell_command" "expected exit 2"

# read_file on a non-exempt file → should mandate (exit 2) since read_file is in edit path? No — read_file is read-only
echo '{"hook_event_name":"PreToolUse","tool_name":"read_file","cwd":"'"$REPO"'","tool_input":{"file_path":"'"$REPO"'/crates/pixel/src/main.rs"}}' | $PIXEL hook guard 2>/dev/null
[ $? -eq 0 ] && ok "Gemini read_file: no block (reads not mandated)" || no "Gemini read_file" "expected exit 0"

# write_file on a non-exempt existing file → should mandate (exit 2)
echo '{"hook_event_name":"PreToolUse","tool_name":"write_file","cwd":"'"$REPO"'","tool_input":{"file_path":"'"$REPO"'/crates/pixel/src/main.rs"}}' | $PIXEL hook guard 2>/dev/null
[ $? -eq 2 ] && ok "Gemini write_file: mandate block (no manifest)" || no "Gemini write_file" "expected exit 2"

# search → should not crash
echo '{"hook_event_name":"PreToolUse","tool_name":"search","cwd":"'"$REPO"'","tool_input":{"pattern":"test"}}' | $PIXEL hook guard 2>/dev/null
[ $? -eq 0 ] && ok "Gemini search: no crash" || no "Gemini search" "expected exit 0"

echo "=== 4. Guard hook — unknown tool name (should pass through) ==="
echo '{"hook_event_name":"PreToolUse","tool_name":"webfetch","cwd":"'"$REPO"'","tool_input":{}}' | $PIXEL hook guard 2>/dev/null
[ $? -eq 0 ] && ok "unknown tool: passthrough" || no "unknown tool" "expected exit 0"

echo "=== 5. Guard hook — non-PreToolUse event (should pass through) ==="
echo '{"hook_event_name":"PostToolUse","tool_name":"exec","cwd":"'"$REPO"'","tool_input":{"command":"git reset --hard"}}' | $PIXEL hook guard 2>/dev/null
[ $? -eq 0 ] && ok "PostToolUse: passthrough" || no "PostToolUse" "expected exit 0"

echo "=== 6. Guard hook — PIXEL_TARGETS_GUARD=0 override ==="
echo '{"hook_event_name":"PreToolUse","tool_name":"Bash","cwd":"'"$REPO"'","tool_input":{"command":"git reset --hard"}}' | PIXEL_TARGETS_GUARD=0 $PIXEL hook guard 2>/dev/null
[ $? -eq 0 ] && ok "PIXEL_TARGETS_GUARD=0: override" || no "override" "expected exit 0"

echo "=== 7. Doctor ==="
$PIXEL doctor 2>&1 | python3 -c "import json,sys; d=json.load(sys.stdin); print('  doctor ok:', d['ok'], '| green:', d['summary']['green'], 'red:', d['summary']['red'])"
$PIXEL doctor 2>&1 | python3 -c "import json,sys; d=json.load(sys.stdin); assert d['ok'], 'doctor not ok'; print('  all checks green')" && ok "doctor: all green" || no "doctor" "not all green"

echo "=== 8. Four mandatory workflows — help surface ==="
$PIXEL targets --help 2>&1 | grep -q "targets" && ok "targets --help" || no "targets --help" "no output"
$PIXEL resolve --help 2>&1 | grep -q "resolve" && ok "resolve --help" || no "resolve --help" "no output"
$PIXEL rescue --help 2>&1 | grep -q "rescue" && ok "rescue --help" || no "rescue --help" "no output"
$PIXEL reconcile --help 2>&1 | grep -q "reconcile" && ok "reconcile --help" || no "reconcile --help" "no output"

echo "=== 9. Devin config wired ==="
python3 -c "
import json
with open('$HOME/.config/devin/config.json') as f:
    d = json.load(f)
hooks = d.get('hooks',{})
ptu = hooks.get('PreToolUse',[])
ss = hooks.get('SessionStart',[])
guard = any('pixel-targets-guard' in h.get('command','') for entry in ptu for h in entry.get('hooks',[]))
session = any('pixel-session-start' in h.get('command','') for entry in ss for h in entry.get('hooks',[]))
assert guard, 'PreToolUse guard not wired in Devin config'
assert session, 'SessionStart not wired in Devin config'
print('  PreToolUse guard:', guard)
print('  SessionStart:', session)
" && ok "Devin config: hooks wired" || no "Devin config" "hooks not wired"

echo "=== 10. Claude settings.json — PreToolUse entry ==="
python3 -c "
import json
with open('$HOME/.claude/settings.json') as f:
    d = json.load(f)
ptu = d.get('hooks',{}).get('PreToolUse',[])
guard = any('pixel-targets-guard' in h.get('command','') for entry in ptu for h in entry.get('hooks',[]))
assert guard, 'PreToolUse guard not wired in Claude settings'
print('  PreToolUse guard wired:', guard)
" && ok "Claude settings: PreToolUse wired" || no "Claude settings" "PreToolUse not wired"

echo "=== 11. Codex hooks.json — PreToolUse entry ==="
python3 -c "
import json
with open('$HOME/.codex/hooks.json') as f:
    d = json.load(f)
ptu = d.get('hooks',{}).get('PreToolUse',[])
guard = any('pixel-targets-guard' in h.get('command','') for entry in ptu for h in entry.get('hooks',[]))
assert guard, 'PreToolUse guard not wired in Codex hooks'
print('  PreToolUse guard wired:', guard)
" && ok "Codex hooks: PreToolUse wired" || no "Codex hooks" "PreToolUse not wired"

echo "=== 12. Gemini settings.json — BeforeTool entry ==="
python3 -c "
import json
with open('$HOME/.gemini/settings.json') as f:
    d = json.load(f)
bt = d.get('hooks',{}).get('BeforeTool',[])
guard = any('pixel-targets-guard' in h.get('command','') for entry in bt for h in entry.get('hooks',[]))
assert guard, 'BeforeTool guard not wired in Gemini settings'
print('  BeforeTool guard wired:', guard)
" && ok "Gemini settings: BeforeTool wired" || no "Gemini settings" "BeforeTool not wired"

echo ""
echo "=== RESULTS ==="
echo "PASS: $PASS  FAIL: $FAIL"
[ $FAIL -eq 0 ] && echo "ALL GREEN" || echo "HAS FAILURES"
