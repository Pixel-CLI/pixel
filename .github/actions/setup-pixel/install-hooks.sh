#!/usr/bin/env bash
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT
set -euo pipefail
args=(setup --yes --json)
case "${PIXEL_ACTION_HOOKS_AGENT:-all}" in
  all) ;;
  *)
    help=$("$PIXEL_ACTION_BIN" setup --help)
    if ! grep -q -- '--agent ' <<< "$help"; then
      echo '::error::Selecting hooks-agent requires a Pixel release supporting setup --agent; setup was not run.' >&2
      exit 1
    fi
    args+=(--agent "$PIXEL_ACTION_HOOKS_AGENT")
    ;;
esac
report=$(mktemp "$RUNNER_TEMP/pixel-setup-report.XXXXXX")
"$PIXEL_ACTION_BIN" "${args[@]}" > "$report"
cat "$report"
python3 -c 'import json,sys; r=json.load(open(sys.argv[1])); sys.exit(0 if r.get("ok") is True else "Pixel hook setup failed")' "$report"
