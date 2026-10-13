# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT
"""Reference implementation of the brief gate's score.

Reads only what the Rust lane has at run time: the `facts.relevance` block of the daemon's `targets_facts`
answer (as JSON), the typed prompt, and `results/gate-model.json`. Every feature below is the definition in
that file; the Rust code must return the same number for the same block and prompt.

    python3 gate_reference.py <gate-model.json> <block.json> "<prompt>"
"""
import json
import math
import re
import sys

IDF_CAP = 4.0
UBIQUITOUS_INVERSE_SHARE = 4


def words(text):
    """Lowercase runs of [a-z0-9_]: everything else separates words."""
    return re.findall(r"[a-z0-9_]+", text.lower())


def row_weight(row, files_considered):
    """pixel_daemon::relevance::row_weight: 0 for a general word or one found in over a quarter of the files,
    IDF_CAP for a word found nowhere, else min(ln((n + 1) / (df + 1)), IDF_CAP), df the largest channel count."""
    if row.get("common"):
        return 0.0
    df = max(row.get("content_files", 0), row.get("symbol_files", 0), row.get("filename_files", 0))
    if df * UBIQUITOUS_INVERSE_SHARE > files_considered:
        return 0.0
    if df == 0:
        return IDF_CAP
    return min(math.log((files_considered + 1) / (df + 1)), IDF_CAP)


def features(model, block, text):
    """The model's features, by name. `text` is the typed prompt with its tagged pasted blocks removed."""
    constants = model["constants"]
    rows = block.get("keywords", [])
    n = block["files_considered"]
    q = sum(row_weight(r, n) for r in rows)
    best_structural = max((c["weight"] for c in block.get("cofiles", []) if c.get("structural")), default=0.0)
    first_words = words(text)[:3]
    return {
        "n_kw": float(len(rows)),
        "question": float(text.strip().endswith("?") or any(w in constants["question_words"] for w in first_words)),
        "ops_share": sum(r["keyword"] in constants["ops_vocab"] for r in rows) / len(rows) if rows else 0.0,
        "struct_ratio": best_structural / q if q > 0 else 0.0,
        "struct_per_mille": math.log1p(1000.0 * block.get("structural_files", 0) / n) if n else 0.0,
    }


def score(model, block, text):
    """The gate's score, or None when there is nothing to score (no block: the prompt had no searchable keyword)."""
    if not block or not block.get("keywords"):
        return None
    values = features(model, block, text)
    return model["intercept_raw"] + sum(f["raw_coefficient"] * values[f["name"]] for f in model["features"])


def tier(model, value):
    if value is None:
        return "off"
    if value > model["thresholds"]["high"]["score"]:
        return "high"
    if value > model["thresholds"]["low"]["score"]:
        return "low"
    return "off"


if __name__ == "__main__":
    model = json.load(open(sys.argv[1]))
    block = json.load(open(sys.argv[2]))
    value = score(model, block, sys.argv[3])
    print(json.dumps({"score": value, "tier": tier(model, value)}))
