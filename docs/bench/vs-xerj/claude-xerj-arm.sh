#!/bin/bash
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

# ARENA_CLAUDE_BIN for a Pixel-vs-xerj arena run. The pixel arm runs Claude Code unchanged (Pixel's brief only).
# The no-pixel arm (PIXEL_BRIEF=0: no Pixel hook, no pixel on PATH) becomes the xerj arm: `xerj init` wires the
# clone the way xerj documents (.mcp.json, .claude/skills/xerj/SKILL.md, AGENTS.md section), XERJ_URL points the
# CLI and the MCP server at the node holding only this repository, and --mcp-config loads the server the arena's
# --strict-mcp-config would otherwise ignore.
REAL=${XERJ_ARM_REAL_CLAUDE:-$HOME/.local/bin/claude}
[ "${PIXEL_BRIEF:-}" = 0 ] || exec "$REAL" "$@"
HERE=/private/tmp/claude-501/-Users-livio-Documents-pixel/03a1fb04-d4e2-4aef-b3e1-5a21609e8863/scratchpad
XDIR=$HERE/xerj-dl/xerj-1.0.0-rc.95-aarch64-apple-darwin
case "$(git rev-parse HEAD 2>/dev/null)" in
  3fce3b5b*) port=9310 ;;
  611c6162*) port=9320 ;;
  541653c7*) port=9330 ;;
  85bede7d*) port=9340 ;;
  *) echo "claude-xerj-arm: no xerj node for $(git rev-parse --short HEAD 2>/dev/null)" >&2; exec "$REAL" "$@" ;;
esac
export XERJ_URL=http://localhost:$port PATH="$XDIR:$PATH"
[ -f .claude/skills/xerj/SKILL.md ] || "$XDIR/xerj" init --url "$XERJ_URL" >>"$HERE/xerj-arm-init.log" 2>&1
echo "$(date -u +%FT%TZ) $(pwd) port=$port" >>"$HERE/xerj-arm-launch.log"
exec "$REAL" "$@" --mcp-config "$PWD/.mcp.json"
