# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT
"""Research: the chunk-feature extractor's input, from the relevance dump: id, text and the
non-common keywords of every dev prompt. usage: make_input.py <dump.json> <out.jsonl>"""
import json, sys
dump = json.load(open(sys.argv[1]))
with open(sys.argv[2], "w") as out:
    for prompt in dump["prompts"]:
        row = prompt["row"]
        out.write(json.dumps({"id": row["id"], "text": row["text"],
                              "noncommon": [k["keyword"] for k in prompt["keywords"] if not k["common"]]}) + "\n")
