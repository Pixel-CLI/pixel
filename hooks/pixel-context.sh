#!/bin/sh
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

# pixel-context.sh — plugin lifecycle hook. Emits the pixel retrieval
# protocol as hookSpecificOutput.additionalContext so plugin installs get
# always-on instructions without `pixel install` or shell wrappers.
#
# Usage: pixel-context.sh <SessionStart|SubagentStart>
# SessionStart injects PIXEL.md (the agent prompt), SubagentStart the short
# PIXEL-SUBAGENT.md, the split `pixel install` makes. Context persists for the
# session; per-prompt re-injection would waste tokens every turn.
#
# The protocol is only injected when it can be followed: without a `pixel`
# on PATH, or with one that predates the command names it uses, the hook
# injects a one-paragraph notice instead of ~4 600 tokens of commands that
# fail.
#
# Non-blocking by contract: reads stdin to EOF (some harnesses require it),
# prints one JSON line, always exits 0. POSIX sh, sed and awk only.

DIR=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
case "${1:-}" in
  SubagentStart) EVENT=SubagentStart FILE="$DIR/PIXEL-SUBAGENT.md" ;;
  *) EVENT=SessionStart FILE="$DIR/PIXEL.md" ;;
esac

cat >/dev/null 2>&1 || :   # drain stdin; never block the harness
[ -f "$FILE" ] || exit 0   # missing context file → stay silent

# Emit `text` (read from stdin) as a JSON string: backslashes and quotes
# escaped, tabs and carriage returns as \t and \r, other control characters
# dropped, lines joined with \n.
json_string() {
  tab=$(printf '\t')
  cr=$(printf '\r')
  tr -d '\000-\010\013\014\016-\037' |
    sed -e 's/\\/\\\\/g' -e 's/"/\\"/g' -e "s/$tab/\\\\t/g" -e "s/$cr/\\\\r/g" |
    awk 'BEGIN { printf "\"" } NR > 1 { printf "\\n" } { printf "%s", $0 } END { printf "\"" }'
}

emit() { # stdin = context text
  printf '{"hookSpecificOutput":{"hookEventName":"%s","additionalContext":' "$EVENT"
  json_string
  printf '}}\n'
}

if ! command -v pixel >/dev/null 2>&1; then
  emit <<'EOF'
The pixel plugin is installed but the `pixel` binary is not on PATH, so its retrieval protocol was not loaded. Tell the user if they ask for pixel commands (install instructions: https://github.com/Pixel-CLI/pixel#for-ai-agents) and use the regular tools meanwhile; do not download or run an installer yourself.
EOF
elif ! pixel repo-state --help >/dev/null 2>&1; then
  version=$(pixel --version 2>/dev/null | awk 'NR == 1 { print $2 }')
  emit <<EOF
The pixel plugin's protocol names commands (such as \`pixel repo-state\`) that the installed pixel ${version:-binary} does not accept, so the protocol was not loaded. Tell the user to upgrade pixel if they ask for pixel commands, and use the regular tools meanwhile.
EOF
else
  emit <"$FILE"
fi
exit 0
