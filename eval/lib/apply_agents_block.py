# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Edit AGENTS.md (cwd) managed pixel block. Args: mode(strip|replace) [body-file]"""
import sys

mode, body_file = sys.argv[1], sys.argv[2] if len(sys.argv) > 2 else ""
text = open("AGENTS.md").read()
BEGIN, END = "<!-- pixel:warp-retrieval:begin -->", "<!-- pixel:warp-retrieval:end -->"
b = text.find(BEGIN)
if b == -1:
    if mode == "strip":
        sys.exit(0)  # already stripped; nothing to do
    print("no managed block found", file=sys.stderr)
    sys.exit(1)
e = text.find(END) + len(END)
if mode == "strip":
    out = text[:b] + text[e:]
elif mode == "replace":
    body = open(body_file).read().strip()
    out = text[:b] + BEGIN + "\n" + body + "\n" + END + text[e:]
else:
    out = text
open("AGENTS.md", "w").write(out)
