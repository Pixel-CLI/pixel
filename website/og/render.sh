#!/usr/bin/env bash
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

# Renders the share card, website/assets/og.png (1200x630, the ratio X,
# LinkedIn and Slack draw), from og/card.html and the token wall's row of
# data/read_savings.toml. Re-run it whenever that row changes: the card's
# figures are the home page's and /benchmarks/', never typed by hand.
#
# Usage: website/og/render.sh
# Needs python3 (3.11+, for tomllib), agent-browser and the network (the
# card loads the site's Google Fonts).
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
site=$(dirname "$here")
out="$site/assets/og.png"
tmp=$(mktemp -d)
session="pixel-og-$$"
trap 'agent-browser --session "$session" close >/dev/null 2>&1 || true; rm -rf "$tmp"' EXIT

# The row the wall draws, as the page script's DATA.
data=$(python3 - "$site/data/read_savings.toml" <<'PY'
import json, sys, tomllib, os
rows = [r for r in tomllib.load(open(sys.argv[1], "rb"))["row"] if r.get("wall")]
if len(rows) != 1:
    sys.exit(f"expected one row marked wall, found {len(rows)}")
r = rows[0]
print(json.dumps({"owner": r["owner"], "file": os.path.basename(r["path"]),
                  "full": r["full"], "pixel": r["pixel"], "saved": r["saved"]}))
PY
)
python3 - "$here/card.html" "$tmp/card.html" "$data" <<'PY'
import sys
src, dst, data = sys.argv[1:]
html = open(src).read()
assert "/*DATA*/" in html
open(dst, "w").write(html.replace("/*DATA*/", "window.DATA = " + data + ";"))
PY

# agent-browser: open first, then size the viewport (it only applies to a
# running browser), and wait for the fonts before the capture.
agent-browser --session "$session" open "file://$tmp/card.html" >/dev/null
agent-browser --session "$session" set viewport 1200 630 >/dev/null
agent-browser --session "$session" wait "[data-ready]" >/dev/null
agent-browser --session "$session" wait 500 >/dev/null
agent-browser --session "$session" screenshot "$tmp/og.png" >/dev/null

# A wrong size would be cropped by every card renderer: refuse it.
python3 - "$tmp/og.png" <<'PY'
import struct, sys
with open(sys.argv[1], "rb") as f:
    head = f.read(24)
w, h = struct.unpack(">II", head[16:24])
if (w, h) != (1200, 630):
    sys.exit(f"og.png is {w}x{h}, expected 1200x630")
PY
mv "$tmp/og.png" "$out"
echo "wrote $out ($data)"
