#!/bin/sh
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

# gen-plugin-assets.sh — regenerate every plugin-manifest surface from the
# canonical agent prompts so each agent CLI can install pixel through its own
# native plugin mechanism (ponytail-style).
#
# Sources of truth:
#   crates/pixel-install/assets/pixel-agent-prompt.md     → every rules/skill surface + PIXEL.md
#   crates/pixel-install/assets/pixel-subagent-prompt.md  → PIXEL-SUBAGENT.md
#
# The plugin hook (hooks/pixel-context.sh) injects PIXEL.md at SessionStart
# and PIXEL-SUBAGENT.md at SubagentStart, the same split `pixel install`
# makes between the agent and the sub-agent prompt.
#
# Usage:
#   scripts/gen-plugin-assets.sh          # write all derived files
#   scripts/gen-plugin-assets.sh --check  # exit 1 if any derived file is stale
set -eu

ROOT=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
SRC="$ROOT/crates/pixel-install/assets/pixel-agent-prompt.md"
SUB_SRC="$ROOT/crates/pixel-install/assets/pixel-subagent-prompt.md"
VERSION=$(grep -m1 '^version' "$ROOT/crates/pixel/Cargo.toml" | sed -E 's/.*"([^"]+)".*/\1/')

[ -f "$SRC" ] || { echo "missing $SRC" >&2; exit 1; }
[ -f "$SUB_SRC" ] || { echo "missing $SUB_SRC" >&2; exit 1; }

# Every derived file, relative to the output root.
FILES="skills/pixel/SKILL.md .agents/skills/pixel/SKILL.md .openclaw/skills/pixel/SKILL.md .cursor/rules/pixel.mdc
.windsurf/rules/pixel.md .kiro/steering/pixel.md .qoder/rules/pixel.md .clinerules/pixel.md
rules/pixel.md PIXEL.md PIXEL-SUBAGENT.md"

# --check mode: render into a scratch dir and compare, without rewriting.
if [ "${1:-}" = "--check" ]; then
  TMP=$(mktemp -d)
  trap 'rm -rf "$TMP"' EXIT
  OUT_ROOT="$TMP" sh "$0" >/dev/null
  stale=0
  for f in $FILES; do
    if ! cmp -s "$ROOT/$f" "$TMP/$f" 2>/dev/null; then
      echo "STALE: $f"
      stale=1
    fi
  done
  [ "$stale" -eq 0 ] && echo "plugin assets in sync (v$VERSION)"
  exit "$stale"
fi

# OUT_ROOT lets --check render into a scratch dir; default is the repo root.
OUT_ROOT=${OUT_ROOT:-$ROOT}

# Header of every agent surface. A plugin can reach a machine without the
# binary; the agent must not fetch and run an installer on its own.
bootstrap() {
  cat <<'EOF'
## Setup — the `pixel` binary is required

Pixel is a CLI, not just instructions. Before relying on any command below,
check that it exists with `command -v pixel`. If it does not, tell the user
that the pixel plugin needs the `pixel` binary (install instructions:
https://github.com/Pixel-CLI/pixel#for-ai-agents) and work without the commands
below; do not download or run an installer yourself.

Make sure the repo is indexed (once per clone/worktree):

    pixel build-index

If `.pixel/` already exists in the repo root, skip straight to the commands.

EOF
}

write_file() { # $1 = rel path, $2 = frontmatter (may be empty), $3 = source, $4 = with bootstrap (yes|no)
  dest="$OUT_ROOT/$1"
  mkdir -p "$(dirname "$dest")"
  tmp="${dest}.tmp.$$"
  {
    if [ -n "$2" ]; then printf '%s\n' "$2"; fi
    if [ "$4" = yes ]; then bootstrap; fi
    cat "$3"
  } >"$tmp"
  mv "$tmp" "$dest"
  echo "wrote $1"
}

SKILL_FRONT='---
name: pixel
description: >
  Deterministic code retrieval: indexed search, concept resolve, impact
  analysis, caller/callee tracing, task scoping, plan generation, and git
  history archaeology via the `pixel` CLI. Use when the repo has a `.pixel`
  directory, when the user mentions pixel, or before editing a symbol when
  blast radius matters. Requires the `pixel` binary on PATH.
license: MIT
---
'

CURSOR_FRONT='---
description: Pixel retrieval layer — optional retrieval helpers for the pixel CLI (search-content, find-code, impact, recall). Requires the pixel binary.
alwaysApply: true
---
'

DEVIN_RULE_FRONT='---
trigger: always_on
---
'

write_file "skills/pixel/SKILL.md"            "$SKILL_FRONT"  "$SRC" yes
write_file ".agents/skills/pixel/SKILL.md"    "$SKILL_FRONT"  "$SRC" yes
write_file ".openclaw/skills/pixel/SKILL.md"  "$SKILL_FRONT"  "$SRC" yes
write_file ".cursor/rules/pixel.mdc"          "$CURSOR_FRONT" "$SRC" yes
write_file ".windsurf/rules/pixel.md"         ""              "$SRC" yes
write_file ".kiro/steering/pixel.md"          ""              "$SRC" yes
write_file ".qoder/rules/pixel.md"            ""              "$SRC" yes
write_file ".clinerules/pixel.md"             ""              "$SRC" yes
write_file "rules/pixel.md"                   "$DEVIN_RULE_FRONT" "$SRC" yes
write_file "PIXEL.md"                         ""              "$SRC" yes
write_file "PIXEL-SUBAGENT.md"                ""              "$SUB_SRC" no

echo "plugin assets generated from the agent prompts (v$VERSION)"
