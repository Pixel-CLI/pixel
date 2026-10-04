#!/usr/bin/env bash
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

# Pi runner for the eval loop. Reads $WT $PROMPT $OUT. v1 limitation: pi's
# pixel integration is a global extension (no per-arm config isolation yet),
# so pi measures the installed state, not per-arm variants. Writes a single
# JSON envelope {status, response} to $OUT for score.py.
set -euo pipefail
: "${WT:?}" "${PROMPT:?}" "${OUT:?}"
cd "$WT"
"${PI_BIN:-pi}" -p "$PROMPT" > "${OUT%.jsonl}.pi.txt" 2> "${OUT%.jsonl}.err"
python3 - "$OUT" "${OUT%.jsonl}.pi.txt" <<'PY'
import json, sys
text = open(sys.argv[2], encoding="utf-8", errors="replace").read()
json.dump({"status": "SUCCESS" if text.strip() else "EMPTY", "response": text,
           "num_turns": None, "usage": None}, open(sys.argv[1], "w"))
PY
