# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT
"""Fit the compact English gate, calibrate its two thresholds on out-of-fold scores and export it.

Features come from the production `facts.relevance` block (the dump's `production`) and the typed prompt only,
through gate_reference.py; the exploration table (gate_compact.py, built from the raw channels) is the cross-check.
Fit and thresholds use the English dev rows; French dev rows are scored and reported as information.

usage: gate_export.py <dump.json> <prompts.jsonl> <out gate-model.json>
"""
import hashlib
import importlib.util
import json
import os
import subprocess
import sys
import warnings

import numpy as np
from sklearn.linear_model import LogisticRegression
from sklearn.model_selection import LeaveOneGroupOut, RepeatedStratifiedKFold
from sklearn.preprocessing import StandardScaler

warnings.filterwarnings("ignore")
HERE = os.path.dirname(os.path.abspath(__file__))


def load_module(name, filename):
    spec = importlib.util.spec_from_file_location(name, os.path.join(HERE, filename))
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


ref = load_module("gate_reference", "gate_reference.py")
GC = load_module("gate_compact", "gate_compact.py")

FEATURES = ["struct_per_mille", "question", "ops_share", "struct_ratio"]
C = 0.3
SEED = 7
REPEATS = 20

DEFINITIONS = {
    "struct_per_mille": {
        "definition": "ln(1 + 1000 * structural_files / files_considered); 0 when files_considered is 0",
        "sources": ["facts.relevance.structural_files", "facts.relevance.files_considered"],
        "why": "how widely the prompt's rare words name files and symbols here, as a share of the repository (the same value at any size)",
    },
    "question": {
        "definition": "1 when the trimmed prompt ends with '?' or one of its first three words is in constants.question_words, else 0",
        "sources": ["the typed prompt, pasted blocks removed"],
        "why": "the shape of a question about the code",
    },
    "ops_share": {
        "definition": "(number of keyword rows whose keyword is in constants.ops_vocab) / (number of keyword rows); 0 with no rows",
        "sources": ["facts.relevance.keywords[].keyword"],
        "why": "git, release and CI operations are not code questions",
    },
    "struct_ratio": {
        "definition": "B / Q, 0 when Q <= 0. B = the largest cofiles[].weight among co-files with structural true (0 if none); "
                      "Q = the sum over keywords[] of row_weight(row), row_weight = 0 when row.common, else with df = max(content_files, "
                      "symbol_files, filename_files): 0 when df * 4 > files_considered, IDF_CAP (4.0) when df is 0, else "
                      "min(ln((files_considered + 1) / (df + 1)), 4.0)",
        "sources": ["facts.relevance.cofiles[].weight", "facts.relevance.cofiles[].structural", "facts.relevance.keywords[]",
                    "facts.relevance.files_considered"],
        "why": "how strongly the rare words, taken together, land on a file named for them",
    },
}


def fit(x, y):
    scaler = StandardScaler().fit(x)
    model = LogisticRegression(C=C, penalty="l2", class_weight="balanced", max_iter=5000).fit(scaler.transform(x), y)
    return scaler, model


def decision(scaler, model, x):
    return model.decision_function(scaler.transform(x))


def metrics(scores, y):
    return GC.metrics(np.asarray(scores, float), y)


def main():
    dump_path, prompts_path, out_path = sys.argv[1:4]
    dump = json.load(open(dump_path))
    stub = {"constants": {"ops_vocab": GC.OPS_VOCAB, "question_words": GC.QUESTION_WORDS}}

    rows = []
    for p in dump["prompts"]:
        row = p["row"]
        visible = GC.PASTE.sub("", row["text"])
        block = None if "<pasted_content" in row["text"] else p["production"]
        feats = ref.features(stub, block, visible) if block and block.get("keywords") else None
        rows.append({"id": row["id"], "text": row["text"], "visible": visible, "block": block, "feats": feats,
                     "on_topic": bool(row["on_topic"]), "source": row["source"], "lang": row["lang"], "kind": row["kind"]})

    # cross-check against the table built from the raw channels
    scratch = os.path.dirname(dump_path)
    symbols, lines = GC.file_stats(f"{scratch}/relevance-fixture/.pixel/graph.v2.db", f"{scratch}/relevance-fixture", dump["paths"])
    table = {t["id"]: t for t in GC.build_table(dump, {}, symbols, lines)}
    worst = 0.0
    for r in rows:
        if r["feats"] is None:
            continue
        explore = table[r["id"]]["values"]
        mapped = {"n_kw": "n_kw", "question": "question", "ops_share": "ops_share", "struct_ratio": "struct_ratio",
                  "struct_per_mille": "struct_per_mille"}
        for name, key in mapped.items():
            worst = max(worst, abs(r["feats"][name] - explore[key]))
    print(f"reference features equal the exploration table: max difference {worst:.2e}")
    assert worst < 1e-9

    en = [r for r in rows if r["lang"] == "en"]
    fr = [r for r in rows if r["lang"] == "fr"]
    # a prompt with nothing to score (no block) is off by construction; it is not part of the fit
    scorable = [r for r in en if r["feats"] is not None]
    unscorable = [r for r in en if r["feats"] is None]
    X = np.array([[r["feats"][n] for n in FEATURES] for r in scorable], float)
    y = np.array([int(r["on_topic"]) for r in scorable])
    groups = np.array([r["source"] for r in scorable])
    print(f"English dev rows: {len(en)} ({len(scorable)} scorable, {len(unscorable)} without a block); {int(y.sum())} on-topic, {int((1 - y).sum())} off-topic")

    # --- out-of-fold scores: repeated stratified 5-fold, mean per prompt ------------------------------------
    oof = np.zeros((REPEATS, len(y)))
    for k, (tr, te) in enumerate(RepeatedStratifiedKFold(n_splits=5, n_repeats=REPEATS, random_state=SEED).split(X, y)):
        scaler, model = fit(X[tr], y[tr])
        oof[k // 5, te] = decision(scaler, model, X[te])
    cv = [metrics(oof[i], y) for i in range(REPEATS)]
    pooled = oof.mean(axis=0)
    loso = np.zeros(len(y))
    for tr, te in LeaveOneGroupOut().split(X, y, groups):
        scaler, model = fit(X[tr], y[tr])
        loso[te] = decision(scaler, model, X[te])
    loso_metrics = metrics(loso, y)

    negatives = np.sort(pooled[y == 0])[::-1]
    high = float(negatives[int(0.05 * len(negatives))])
    low = float(negatives[int(0.10 * len(negatives))])

    def operating(scores, threshold):
        return {"tpr": float((scores[y == 1] > threshold).mean()), "fpr": float((scores[y == 0] > threshold).mean()),
                "false_positives": int((scores[y == 0] > threshold).sum()), "true_positives": int((scores[y == 1] > threshold).sum())}

    # --- the final model, fit on every scorable English dev row ---------------------------------------------------
    scaler, model = fit(X, y)
    coef = model.coef_[0]
    raw = coef / scaler.scale_
    intercept_raw = float(model.intercept_[0] - np.sum(coef * scaler.mean_ / scaler.scale_))
    out = {
        "name": "english compact gate, four features",
        "scope": "English prompts only; French dev rows are scored for information and were not used",
        "fit": {
            "relevance_commit": subprocess.check_output(["git", "rev-parse", "HEAD"], text=True, cwd=HERE).strip(),
            "tracked_files_clean": subprocess.check_output(["git", "status", "--porcelain", "--untracked-files=no"], text=True, cwd=HERE) == "",
            "prompts_path": "eval/brief-gate/prompts.jsonl (PR #884, commit dd0f68d5)",
            "prompts_sha256": hashlib.sha256(open(prompts_path, "rb").read()).hexdigest(),
            "fixture_sha": "85bede7d9a3c4e385e0a2045241a6466efbd8cbd",
            "split": "dev",
            "rows": {"english": len(en), "scored_in_fit": len(scorable), "english_without_block": len(unscorable),
                     "on_topic": int(y.sum()), "off_topic": int((1 - y).sum()), "french_info_only": len(fr)},
            "estimator": f"L2 logistic regression, C={C}, class_weight balanced, features standardised on the fit rows, lbfgs",
            "validation": f"stratified 5-fold repeated {REPEATS} times (seed {SEED}), and leave-one-source-out",
            "commands": [
                "git worktree add --detach <fixture> 85bede7d9a3c4e385e0a2045241a6466efbd8cbd && pixel prepare-repo <fixture>",
                "PIXEL_WEIGHT_FIXTURE=<fixture> PIXEL_WEIGHT_PROMPTS=<prompts.jsonl> PIXEL_WEIGHT_SPLIT=dev PIXEL_WEIGHT_OUT=<dump.json> "
                "cargo test -p pixel-daemon dump_the_raw_material -- --ignored",
                "python3 scripts/research-gate/gate_export.py <dump.json> <prompts.jsonl> scripts/research-gate/results/gate-model.json",
            ],
        },
        "score": "intercept_raw + sum(raw_coefficient[i] * feature[i]); a log-odds under a balanced prior, not a calibrated probability",
        "intercept_raw": intercept_raw,
        "features": [
            {"name": n, **DEFINITIONS[n], "mean": float(scaler.mean_[i]), "std": float(scaler.scale_[i]),
             "coefficient": float(coef[i]), "raw_coefficient": float(raw[i])}
            for i, n in enumerate(FEATURES)
        ],
        "intercept_standardised": float(model.intercept_[0]),
        "thresholds": {
            "high": {"score": high, "target_fpr": 0.05, "rule": "score > high: full brief",
                     "out_of_fold": operating(pooled, high), "leave_one_source_out": operating(loso, high)},
            "low": {"score": low, "target_fpr": 0.10, "rule": "high >= score > low: compact 'possibly related' brief; score <= low: none",
                    "out_of_fold": operating(pooled, low), "leave_one_source_out": operating(loso, low)},
            "calibration": "the 5 % and 10 % points of the pooled out-of-fold scores of the off-topic English dev rows "
                           f"({len(negatives)} of them: at most {int(0.05 * len(negatives))} and {int(0.10 * len(negatives))} above); "
                           "a prompt is above a threshold when its score is strictly greater",
        },
        "metrics": {
            "cv_mean": {k: float(np.mean([m[k] for m in cv])) for k in ("auc", "tpr5", "tpr10")},
            "cv_sd": {k: float(np.std([m[k] for m in cv])) for k in ("auc", "tpr5", "tpr10")},
            "leave_one_source_out": loso_metrics,
        },
        "constants": {
            "ops_vocab": GC.OPS_VOCAB,
            "question_words": GC.QUESTION_WORDS,
            "idf_cap": 4.0,
            "ubiquitous_inverse_share": 4,
            "words": "lowercase runs of [a-z0-9_] in the prompt",
        },
        "edge_cases": [
            "no block, or a block without keywords (tokenize_task found nothing to search): the prompt is off, no score",
            "the typed prompt has its <pasted_content ...>...</pasted_content> blocks (and an unclosed trailing one) removed before 'question' is read; the block must be computed from the same text",
            "the fit rows all had graph: true; without a graph structural_files counts path matches only and the model should not be used",
            "row_weight is pixel_daemon::relevance::row_weight; 'common' is the row's flag",
        ],
    }

    # --- the exported JSON, scored through the reference implementation, equals the fitted model -----------------
    ref_scores = np.array([ref.score(out, r["block"], r["visible"]) for r in scorable])
    direct = decision(scaler, model, X)
    gap = float(np.max(np.abs(ref_scores - direct)))
    print(f"reference scorer on the exported JSON equals the fitted model: max difference {gap:.2e}")
    assert gap < 1e-9

    # --- information: the French dev rows, and the English rows with no block -----------------------------------------
    def tiers(group):
        counts = {"high": 0, "low": 0, "off": 0}
        for r in group:
            counts[ref.tier(out, ref.score(out, r["block"], r["visible"]))] += 1
        return counts

    info = {}
    for label, group in (("french_on_topic", [r for r in fr if r["on_topic"]]), ("french_off_topic", [r for r in fr if not r["on_topic"]]),
                         ("english_on_topic", [r for r in en if r["on_topic"]]), ("english_off_topic", [r for r in en if not r["on_topic"]])):
        info[label] = {"rows": len(group), "tiers": tiers(group)}
    out["information"] = {
        "tiers_at_the_two_thresholds": info,
        "note": "the French rows are scored by the English model and were not used to fit or to calibrate",
    }
    json.dump(out, open(out_path, "w"), indent=2)
    print(json.dumps({k: out[k] for k in ("intercept_raw",)}))
    for f in out["features"]:
        print(f"  {f['name']:18} mean {f['mean']:.4f} std {f['std']:.4f} coef {f['coefficient']:+.4f} raw {f['raw_coefficient']:+.4f}")
    print("thresholds:", {k: round(v["score"], 4) for k, v in out["thresholds"].items() if k in ("high", "low")})
    for k in ("high", "low"):
        print(k, "out-of-fold", out["thresholds"][k]["out_of_fold"], "LOSO", out["thresholds"][k]["leave_one_source_out"])
    print("metrics", out["metrics"])
    print("information", json.dumps(info))


if __name__ == "__main__":
    main()
