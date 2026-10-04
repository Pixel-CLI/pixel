# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Stdin: Claude settings JSON. Stdout: same JSON with every pixel-ish hook entry and enabled Pixel plugin removed."""
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
# A Pixel plugin enabled in the operator's settings would load its hooks and
# skill through the linked plugins directory: disable it, keep the others.
plugins = s.get("enabledPlugins")
if isinstance(plugins, dict):
    s["enabledPlugins"] = {name: on for name, on in plugins.items() if "pixel" not in name.lower()}
print(json.dumps(s))
