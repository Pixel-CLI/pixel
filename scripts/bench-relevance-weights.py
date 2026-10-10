#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT
"""Compare co-file weighting rules on a labelled prompt set.

`facts.relevance` ranks the files the words of a prompt meet in by the weight
of those words (`pixel_daemon::relevance::keyword_weight`). This script holds
the candidate rules next to each other and measures them on the dev split of
`eval/brief-gate/prompts.jsonl` (issue #883), so the rule in the code is chosen
by a number and not by two examples.

It does not search a repository itself. The Rust test
`dump_the_raw_material_of_the_co_file_ranking` writes, for every prompt, the
files each keyword matched per channel (content, symbol, path) in a checkout
the labels were read against, plus what the code returns today. This script
re-ranks that material under each rule:

    FIX=$(git rev-parse 85bede7d)            # FIXTURE_SHA of the prompt set
    git worktree add --detach /tmp/fixture $FIX
    PIXEL_WEIGHT_FIXTURE=/tmp/fixture \\
    PIXEL_WEIGHT_PROMPTS=eval/brief-gate/prompts.jsonl \\
    PIXEL_WEIGHT_SPLIT=dev PIXEL_WEIGHT_OUT=/tmp/dump-dev.json \\
        cargo test -p pixel-daemon dump_the_raw_material -- --ignored
    python3 scripts/bench-relevance-weights.py /tmp/dump-dev.json

`--verify` checks that the chosen rule, simulated here, returns exactly the
co-files the code returned (the dump carries them): the script and the code
spell one rule, or the check says so. The general words (`common`) come from
the dump, that is from the list in the code. Tune on `dev`; read `test` once, after
the choice (eval/brief-gate/README.md).
"""

import argparse
import json
import math
import sys

IDF_CAP = 4.0
UBIQUITOUS_INVERSE_SHARE = 4
WEIGHT_DECIMALS = 3


class Rule:
    """One weighting rule. The defaults are the rule in the code."""

    def __init__(self, name, common=False, truncation="zero", boost=1.0,
                 by_weight=5, structural=3, boost_kinds=("symbol", "path"),
                 structure="any"):
        self.name = name
        # What makes a match structural: "any" (a symbol or a path), "path"
        # (a path only), "pair" (a path, or symbols named for two keywords).
        self.structure = structure
        self.common = common
        self.truncation = truncation  # "zero": truncated weighs 0; "bound": df is a lower bound
        self.boost = boost
        self.by_weight = by_weight
        self.structural = structural
        self.boost_kinds = boost_kinds


def keyword_weight(df, n, truncated, rule):
    if rule.truncation == "zero" and truncated:
        return 0.0
    if df * UBIQUITOUS_INVERSE_SHARE > n:
        return 0.0
    if df == 0:
        return IDF_CAP
    return min(math.log((n + 1) / (df + 1)), IDF_CAP)


def rounded(x):
    return round(x * 10**WEIGHT_DECIMALS) / 10**WEIGHT_DECIMALS


def rank_key(candidate):
    weight, structural, path = candidate
    return (-weight, not structural, path)


def rank(prompt, n, paths, rule, common_words):
    """Returns (weights per keyword, listed co-files as (path, weight, structural))."""
    keywords = prompt["keywords"]
    weights = []
    for k in keywords:
        df = max(len(k["content"]), len(k["symbol"]), len(k["filename"]))
        w = keyword_weight(df, n, k["truncated"], rule)
        if rule.common and (k["common"] if common_words is None else k["keyword"] in common_words):
            w = 0.0
        weights.append(w)
    files = {}
    for at, k in enumerate(keywords):
        kinds = {"content": k["content"], "symbol": k["symbol"], "path": k["filename"]}
        for kind, ids in kinds.items():
            for pid in ids:
                entry = files.setdefault(pid, {})
                entry.setdefault(at, set()).add(kind)
    candidates = []
    for pid, per_keyword in files.items():
        total = 0.0
        structural = False
        symbol_keywords = 0
        for at, kinds in per_keyword.items():
            w = weights[at]
            # A name or symbol is structure only when the word that matched it
            # says something.
            if rule.structure == "any":
                structural |= w > 0.0 and bool(kinds & {"symbol", "path"})
            elif rule.structure == "path":
                structural |= w > 0.0 and "path" in kinds
            else:
                structural |= w > 0.0 and "path" in kinds
                symbol_keywords += w > 0.0 and "symbol" in kinds
            total += w * (rule.boost if kinds & set(rule.boost_kinds) else 1.0)
        if rule.structure == "pair" and symbol_keywords >= 2:
            structural = True
        total = rounded(total)
        if total > 0.0:
            candidates.append((total, structural, paths[pid]))
    candidates.sort(key=rank_key)
    listed = []
    structural_seen = 0
    for at, (w, structural, path) in enumerate(candidates):
        if structural:
            structural_seen += 1
        if at < rule.by_weight or (structural and structural_seen <= rule.structural):
            listed.append((path, w, structural))
    return weights, listed


def auc(positive, negative):
    if not positive or not negative:
        return float("nan")
    wins = sum((p > q) + 0.5 * (p == q) for p in positive for q in negative)
    return wins / (len(positive) * len(negative))


def auc_se(value, positives, negatives):
    """Hanley and McNeil's standard error of an AUC."""
    q1 = value / (2 - value)
    q2 = 2 * value * value / (1 + value)
    variance = (value * (1 - value) + (positives - 1) * (q1 - value * value)
                + (negatives - 1) * (q2 - value * value)) / (positives * negatives)
    return math.sqrt(max(variance, 0.0))


def dprime(positive, negative):
    """Distance between the class means in pooled standard deviations: a
    separability that does not move when every score is multiplied."""
    def stats(values):
        mean = sum(values) / len(values)
        return mean, sum((v - mean) ** 2 for v in values) / (len(values) - 1)
    (pm, pv), (nm, nv) = stats(positive), stats(negative)
    pooled = math.sqrt((pv + nv) / 2)
    return (pm - nm) / pooled if pooled else float("nan")


def tpr_at_fp(positive, negative, fp_rate):
    """Share of positives scoring above the threshold that lets at most
    `fp_rate` of the negatives through."""
    allowed = int(fp_rate * len(negative))
    threshold = sorted(negative, reverse=True)[allowed]
    return sum(p > threshold for p in positive) / len(positive)


def percentile(values, p):
    ordered = sorted(values)
    return ordered[min(len(ordered) - 1, round(p * (len(ordered) - 1)))]


def measure(dump, rule, common_words):
    n = dump["files_considered"]
    paths = dump["paths"]
    on_scores, off_scores = [], []
    off_exops = []
    on_raw, off_raw = [], []
    expected_rows = 0
    hit_rows = {}
    hit_returned = hit_structural = 0
    returned_total = 0
    off_best = []
    on_best = []
    off_with_structural = 0
    rows = 0
    for prompt in dump["prompts"]:
        row = prompt["row"]
        if not prompt["keywords"]:
            weights, listed = [], []
        else:
            weights, listed = rank(prompt, n, paths, rule, common_words)
        q = sum(weights)
        best = max((w for _, w, s in listed if s), default=0.0)
        score = best / q if q > 0 else 0.0
        rows += 1
        returned_total += len(listed)
        if row["on_topic"]:
            on_scores.append(score)
            on_raw.append(best)
            on_best.append(best)
        else:
            off_scores.append(score)
            if row["kind"] != "ops":
                off_exops.append(score)
            off_raw.append(best)
            off_best.append(best)
            off_with_structural += best > 0.0
        expected = row.get("expected_files")
        if row["on_topic"] and expected:
            expected_rows += 1
            returned = {path for path, _, _ in listed}
            structural = {path for path, _, s in listed if s}
            hit_returned += bool(returned & set(expected))
            hit_structural += bool(structural & set(expected))
            hit_rows[row["id"]] = bool(returned & set(expected))
    if len(on_scores) < 2 or len(off_scores) < 2:
        # Every class mean, d' (a sample variance) and the TPR divide by
        # these counts: name the split instead of a ZeroDivisionError.
        sys.exit(f"{rule.name}: the split needs at least two on-topic and two "
                 f"off-topic prompts, got {len(on_scores)} and {len(off_scores)}")
    return {
        "rule": rule.name,
        "rows": rows,
        "expected_rows": expected_rows,
        "hit_returned": hit_returned / expected_rows if expected_rows else float("nan"),
        "hit_structural": hit_structural / expected_rows if expected_rows else float("nan"),
        "returned_mean": returned_total / rows,
        "off_best_mean": sum(off_best) / len(off_best),
        "off_best_p90": percentile(off_best, 0.9),
        "off_best_max": max(off_best),
        "off_with_structural": off_with_structural / len(off_best),
        "on_best_mean": sum(on_best) / len(on_best),
        "hit_rows": hit_rows,
        "pos_score": sum(on_scores) / len(on_scores),
        "neg_score": sum(off_scores) / len(off_scores),
        "dprime": dprime(on_scores, off_scores),
        "auc": auc(on_scores, off_scores),
        "auc_raw": auc(on_raw, off_raw),
        "auc_exops": auc(on_scores, off_exops),
        "auc_se": auc_se(auc(on_scores, off_scores), len(on_scores), len(off_scores)),
        "tpr_fp5": tpr_at_fp(on_scores, off_scores, 0.05),
        "tpr_fp10": tpr_at_fp(on_scores, off_scores, 0.10),
    }


# The rule in the code. `--verify` checks it against the code's own output.
CHOSEN = "V1+V2+V4"

RULES = [
    Rule("V0 18f566cc"),
    Rule("V1 common words", common=True),
    Rule("V2 truncation=bound", truncation="bound"),
    Rule("V3 boost 1.5", boost=1.5),
    Rule("V3 boost 2", boost=2.0),
    Rule("V3 boost 3", boost=3.0),
    Rule("V4 union 8+4", by_weight=8, structural=4),
    Rule("V1+V2+V4", common=True, truncation="bound", by_weight=8, structural=4),
    Rule("V1+V2", common=True, truncation="bound"),
    Rule("V1+V2+V3(2)", common=True, truncation="bound", boost=2.0),
    Rule("V1+V2+V3(3)", common=True, truncation="bound", boost=3.0),
    Rule("V1+V2+V3(2)+V4", common=True, truncation="bound", boost=2.0, by_weight=8, structural=4),
    Rule("V2+V3(2)", truncation="bound", boost=2.0),
    Rule("V1+V3(2)", common=True, boost=2.0),
    Rule("V5 path-only", structure="path"),
    Rule("V6 path|2 symbols", structure="pair"),
    Rule("V1+V2+V5", common=True, truncation="bound", structure="path"),
    Rule("V1+V2+V6", common=True, truncation="bound", structure="pair"),
    Rule("V1+V2+V3(2)+V6", common=True, truncation="bound", boost=2.0, structure="pair"),
    Rule("V1+V2+V3(2)+V5", common=True, truncation="bound", boost=2.0, structure="path"),
]


def table(results):
    base = results[0]["hit_rows"]
    for r in results:
        r["gain"] = sum(r["hit_rows"][k] and not base[k] for k in base)
        r["loss"] = sum(base[k] and not r["hit_rows"][k] for k in base)
    head = ("rule", "hit@ret", "hit@str", "ret", "neg mean", "neg p90", "neg max", "neg>0",
            "pos mean", "AUC", "+-se", "AUCraw", "AUCnoOps", "TPR@5", "TPR@10", "d'", "vsV0")
    print(" | ".join(f"{h:>16}" if i == 0 else f"{h:>8}" for i, h in enumerate(head)))
    for r in results:
        cells = [
            f"{r['rule']:>16}", f"{r['hit_returned']:8.3f}", f"{r['hit_structural']:8.3f}",
            f"{r['returned_mean']:8.2f}", f"{r['off_best_mean']:8.3f}", f"{r['off_best_p90']:8.3f}",
            f"{r['off_best_max']:8.3f}", f"{r['off_with_structural']:8.3f}", f"{r['on_best_mean']:8.3f}",
            f"{r['auc']:8.3f}", f"{r['auc_se']:8.3f}", f"{r['auc_raw']:8.3f}", f"{r['auc_exops']:8.3f}",
            f"{r['tpr_fp5']:8.3f}", f"{r['tpr_fp10']:8.3f}", f"{r['dprime']:8.2f}",
            f"{'+%d/-%d' % (r['gain'], r['loss']):>8}",
        ]
        print(" | ".join(cells))


def verify(dump, rule, common_words):
    n = dump["files_considered"]
    mismatches = 0
    for prompt in dump["prompts"]:
        production = prompt["production"]
        if not production:
            continue
        _, listed = rank(prompt, n, dump["paths"], rule, common_words)
        expected = [(c["path"], c["weight"], c["structural"]) for c in production.get("cofiles", [])]
        got = [(path, w, s) for path, w, s in listed]
        if expected != got:
            mismatches += 1
            if mismatches <= 3:
                print(f"MISMATCH {prompt['row']['id']}:\n  code {expected}\n  here {got}")
    print(f"{rule.name}: {mismatches} of {len(dump['prompts'])} prompts differ from the code")
    return mismatches == 0


def self_test():
    """Known answers for the arithmetic, and one toy ranking."""
    assert keyword_weight(0, 100, False, Rule("t")) == IDF_CAP
    assert keyword_weight(25, 100, False, Rule("t")) > 1.0  # a quarter exactly
    assert keyword_weight(26, 100, False, Rule("t")) == 0.0
    assert keyword_weight(5, 100, True, Rule("t")) == 0.0  # zero rule
    assert keyword_weight(5, 100, True, Rule("t", truncation="bound")) > 0.0
    assert auc([2, 3], [1, 2]) == (1 + 1 + 0.5 + 1) / 4
    assert tpr_at_fp([3, 3, 0], [1, 2, 3, 4], 0.0) == 0.0
    assert tpr_at_fp([5, 5, 0], [1, 2, 3, 4], 0.25) == 2 / 3
    assert percentile([1, 2, 3, 4], 0.9) == 4
    assert rounded(1.2346) == 1.235
    toy = {"keywords": [
        {"keyword": "a", "truncated": False, "common": False, "content": [0, 1], "symbol": [], "filename": [], },
        {"keyword": "b", "truncated": False, "common": True, "content": [1, 2], "symbol": [2], "filename": []},
    ]}
    weights, listed = rank(toy, 100, ["x.md", "y.md", "z.rs"], Rule("t", common=True), None)
    assert weights[1] == 0.0 and weights[0] > 0.0
    assert [path for path, _, _ in listed] == ["x.md", "y.md"], listed  # z.rs only matched a general word
    print("self-test ok")
    return 0


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("dump", nargs="?", help="JSON written by dump_the_raw_material_of_the_co_file_ranking")
    parser.add_argument("--self-test", action="store_true", help="check the arithmetic and exit")
    parser.add_argument("--common-words", help="file of words (one per line) replacing the dump's `common` flags")
    parser.add_argument("--verify", nargs="?", const=CHOSEN, metavar="RULE",
                        help=f"check a rule against the code's own output (default {CHOSEN})")
    args = parser.parse_args()
    if args.self_test:
        return self_test()
    if not args.dump:
        parser.error("a dump file is required")
    with open(args.dump) as handle:
        dump = json.load(handle)
    common_words = None
    if args.common_words:
        with open(args.common_words) as handle:
            common_words = {line.strip() for line in handle if line.strip() and not line.startswith("#")}
    if args.verify:
        rule = next(r for r in RULES if r.name == args.verify)
        return 0 if verify(dump, rule, common_words) else 1
    table([measure(dump, rule, common_words) for rule in RULES])
    return 0


if __name__ == "__main__":
    sys.exit(main())
