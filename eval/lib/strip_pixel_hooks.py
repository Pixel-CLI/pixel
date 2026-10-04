# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Stdin: Claude settings JSON. Stdout: same JSON with every pixel-ish hook entry removed."""
import json, sys

s = json.load(sys.stdin)
for event in list(s.get("hooks", {})):
    groups = s["hooks"][event]
    kept = []
    for g in groups:
        hs = [h for h in g.get("hooks", []) if "pixel" not in h.get("command", "")]
        if hs:
            g = dict(g)
            g["hooks"] = hs
            kept.append(g)
    if kept:
        s["hooks"][event] = kept
    else:
        del s["hooks"][event]
print(json.dumps(s))
