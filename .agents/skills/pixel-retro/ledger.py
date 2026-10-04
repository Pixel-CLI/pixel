# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Append retro items to ~/.local/state/pixel-retro/seen.tsv from a JSON file.

Usage: python3 ledger.py <items.json>
The file holds a list of {"fingerprint": str, "verdict": str, "ref": str}.
Values stay data end to end: they are read from JSON, validated, and written
with tabs and newlines replaced, never pasted into shell or Python source.
"""
import datetime, json, os, re, sys

VERDICTS = {"suggested", "picked", "fixed", "wontfix", "not-pixel"}
REF = re.compile(r"[a-z]+:[A-Za-z0-9._-]+ #\d+|https://github\.com/\S+")


def clean(value):
    return re.sub(r"[\t\r\n]", " ", value)


FIELDS = ("fingerprint", "verdict", "ref")


def valid(item):
    return (
        isinstance(item, dict)
        and all(isinstance(item.get(k), str) and item[k] for k in FIELDS)
        and item["verdict"] in VERDICTS
        and REF.fullmatch(item["ref"]) is not None
    )


with open(sys.argv[1]) as src:
    items = json.load(src)
if not isinstance(items, list) or not items:
    sys.exit("refused, nothing written: the file must hold a non-empty JSON list")
for item in items:
    if not valid(item):
        sys.exit(f"refused, nothing written: {item!r}")
path = os.path.expanduser("~/.local/state/pixel-retro/seen.tsv")
os.makedirs(os.path.dirname(path), exist_ok=True)
today = datetime.date.today().isoformat()
with open(path, "a") as f:
    for item in items:
        f.write(f"{today}\t{clean(item['fingerprint'])}\t{item['verdict']}\t{item['ref']}\n")
print(f"{len(items)} row(s) appended to {path}")
