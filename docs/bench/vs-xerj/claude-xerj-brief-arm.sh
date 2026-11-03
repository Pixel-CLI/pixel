#!/bin/bash
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

# ARENA_CLAUDE_BIN for the xerj-brief run. In the hooked arm, whose brief xerj-brief.py replaces, the `pixel` shim is
# taken off PATH so nothing from Pixel reaches the agent (the hooks call their binary by absolute path). The no-pixel
# arm runs unchanged.
REAL=${XERJ_ARM_REAL_CLAUDE:-$HOME/.local/bin/claude}
if [ "${PIXEL_BRIEF:-}" != 0 ]; then
  PATH=$(printf '%s' "$PATH" | tr ':' '\n' | grep -v '/pixel/shim$' | paste -sd: -)
  export PATH
fi
exec "$REAL" "$@"
