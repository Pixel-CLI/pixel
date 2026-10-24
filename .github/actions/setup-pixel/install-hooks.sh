#!/usr/bin/env bash
# SPDX-FileCopyrightText: The RTO contributors
# SPDX-License-Identifier: MIT
set -euo pipefail
args=(setup --yes --json)
case "${PIXEL_ACTION_HOOKS_AGENT:-all}" in
  all) ;;
  *) args+=(--agent "$PIXEL_ACTION_HOOKS_AGENT") ;;
esac
report=$(mktemp "$RUNNER_TEMP/pixel-setup-report.XXXXXX")
"$PIXEL_ACTION_BIN" "${args[@]}" > "$report"
cat "$report"
python3 -c 'import json,sys; r=json.load(open(sys.argv[1])); sys.exit(0 if r.get("ok") is True else "PIXEL hook setup failed")' "$report"
