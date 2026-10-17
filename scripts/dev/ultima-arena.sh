#!/usr/bin/env bash
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT
set -euo pipefail

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)"

case "${1:-}" in
    abort|--help|-h|--self-test)
        ;;
    *)
        if [[ -z "${HERDR_PANE_ID:-}" ]]; then
            printf '%s\n' \
                'ultima-arena: run this from a Herdr operator pane, or set HERDR_PANE_ID to that pane id.' \
                'Use `scripts/dev/ultima-arena.sh --help` for the harness options.' >&2
            exit 2
        fi
        ;;
esac

export HERDR_ENV=1
exec python3 -I "$repo_root/.agents/skills/ultima-arena/ultima_arena.py" \
    --repo "$repo_root" "$@"
