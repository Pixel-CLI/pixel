#!/usr/bin/env bash
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT
# Create the indexed fixture checkout the playground runs `pixel brief` in:
# the pixel repository at 85bede7d, cloned without hardlinks, then indexed.
# Usage: scripts/setup-fixture.sh [dest] [pixel-binary]
set -euo pipefail
dest="${1:-/private/tmp/claude-501/playground/fixture/pixel}"
bin="${2:-pixel}"
rev=85bede7d
src="$(git rev-parse --show-toplevel)"
if [ -d "$dest/.git" ]; then
  echo "fixture exists: $dest"
else
  mkdir -p "$(dirname "$dest")"
  git clone --no-local --quiet "$src" "$dest"
  git -C "$dest" checkout --quiet "$rev"
fi
(cd "$dest" && "$bin" prepare-repo .)
echo "fixture ready: $dest (PLAYGROUND_REPO=$dest)"
