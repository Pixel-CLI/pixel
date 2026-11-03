#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT
"""Refit the brief gate on several repositories (#906) and calibrate its thresholds out of fold.

Input: `results/multirepo-features.json`, one row per (prompt, repository) with the gate's raw features as `pixel
brief --json` logs them (the production features, so the fit sees what the Rust gate sees), from
`eval/brief-gate/prompts-multirepo.jsonl` (set `mr`) and the English rows of `eval/brief-gate/prompts.jsonl`
(set `px`, Pixel only). A prompt that names code (`signal` "strong") skips the gate and has no features: not a row.

The model keeps the four features of the first gate (`gate-model-pixel-only.json`) and adds `coverage`, the share
of the informative keywords the heaviest co-file holds, which does not depend on the size of the repository.
The fit is the same as the first one: standardised features, L2 logistic regression C=0.3 with balanced classes,
repeated 5-fold (20x, grouped by prompt text) out-of-fold scores, HIGH and LOW at the 5 % and 10 % points of the
pooled out-of-fold scores of the off-topic dev rows. `test` is scored once, after the choice.

usage: gate_multirepo.py <multirepo-features.json> <out.json> [--features a,b,c] [--template gate-model-pixel-only.json]

With `--template` the output is a full `gate-model.json` (the template's constants and edge cases, the new fit);
without it, the bare coefficients.
"""
import json
import math
import re
import sys
import warnings

import numpy as np
from sklearn.linear_model import LogisticRegression
from sklearn.model_selection import StratifiedGroupKFold
from sklearn.preprocessing import StandardScaler

warnings.filterwarnings("ignore")
C, REPEATS = 0.3, 20
DEFAULT = ["struct_per_mille", "question", "ops_share", "struct_ratio", "coverage"]


def value(row, name):
    f = row["features"]
    if name == "question":
        return float(f["question"])
    if name == "coverage":
        return f["shared"] / f["informative"] if f["informative"] else 0.0
    return float(f[name])


def matrix(rows, names):
    return (np.array([[value(r, n) for n in names] for r in rows]), np.array([int(r["on_topic"]) for r in rows]))


def weights(rows):
    """An off-topic multirepo prompt is scored against every repository: its four rows count once."""
    return np.array([0.25 if r["set"] == "mr" and not r["on_topic"] else 1.0 for r in rows])


def threshold(off_scores, off_weights, fpr):
    """The lowest score with at most `fpr` of the off-topic weight above it (a score is above when strictly greater)."""
    order = np.argsort(-off_scores)
    allowed = fpr * off_weights.sum() + 1e-9
    above = 0.0
    for position, index in enumerate(order):
        if above + off_weights[index] > allowed:
            return float(off_scores[index])
        above += off_weights[index]
    return float(off_scores[order[-1]] - 1)


def rates(scores, y, cut, w):
    on, off = y == 1, y == 0
    return (float(np.sum(w[on] * (scores[on] > cut)) / np.sum(w[on])),
            float(np.sum(w[off] * (scores[off] > cut)) / np.sum(w[off])))


def auc(scores, y, w):
    pos, neg = scores[y == 1], scores[y == 0]
    wp, wn = w[y == 1], w[y == 0]
    wins = [[(p > n) + 0.5 * (p == n) for n in neg] for p in pos]
    return float(np.sum(np.array(wins) * wp[:, None] * wn[None, :]) / (wp.sum() * wn.sum()))


def fit(x, y, w):
    scaler = StandardScaler().fit(x)
    balance = {1: 0.5 * w.sum() / w[y == 1].sum(), 0: 0.5 * w.sum() / w[y == 0].sum()}
    model = LogisticRegression(C=C, max_iter=5000).fit(
        scaler.transform(x), y, sample_weight=w * np.array([balance[int(label)] for label in y]))
    return scaler, model


def raw_model(scaler, model):
    """Coefficients and intercept in the units of the raw features."""
    raw = model.coef_[0] / scaler.scale_
    return float(model.intercept_[0] - np.sum(raw * scaler.mean_)), [float(c) for c in raw], scaler


def out_of_fold(x, y, w, groups):
    total = np.zeros(len(y))
    for seed in range(REPEATS):
        for train, test in StratifiedGroupKFold(5, shuffle=True, random_state=seed).split(x, y, groups):
            scaler, model = fit(x[train], y[train], w[train])
            total[test] += model.decision_function(scaler.transform(x[test]))
    return total / REPEATS


def report(label, scores, y, w, high, low):
    t_hi, f_hi = rates(scores, y, high, w)
    t_lo, f_lo = rates(scores, y, low, w)
    return (f"{label:34} n={int(y.sum()):3}+/{w[y == 0].sum():5.1f}-  AUC {auc(scores, y, w):.3f}  "
            f"high TPR {t_hi:.2f} FPR {f_hi:.2f}  >=low TPR {t_lo:.2f} FPR {f_lo:.2f}")


def main(argv):
    data = [r for r in json.load(open(argv[0])) if r["features"] and r.get("lang", "en") == "en"]
    names = DEFAULT
    if "--features" in argv:
        names = argv[argv.index("--features") + 1].split(",")
    dev = [r for r in data if r["split"] == "dev"]
    test = [r for r in data if r["split"] == "test"]
    x, y = matrix(dev, names)
    w = weights(dev)
    oof = out_of_fold(x, y, w, [r["text"] for r in dev])
    high = threshold(oof[y == 0], w[y == 0], 0.05)
    low = threshold(oof[y == 0], w[y == 0], 0.10)
    scaler, model = fit(x, y, w)
    intercept, coefficients, scaler = raw_model(scaler, model)
    print("features", names)
    print("raw coefficients", dict(zip(names, [round(c, 4) for c in coefficients])), "intercept", round(intercept, 4))
    print("HIGH", round(high, 4), "LOW", round(low, 4))
    print(report("dev out-of-fold (all)", oof, y, w, high, low))
    for label, keep in (("  multirepo", "mr"), ("  pixel set", "px")):
        mask = np.array([r["set"] == keep for r in dev])
        print(report("dev out-of-fold" + label, oof[mask], y[mask], w[mask], high, low))
    xt, yt = matrix(test, names)
    wt = weights(test)
    scores = np.array([intercept + float(np.dot(coefficients, row)) for row in xt])
    print("--- test, read once ---")
    print(report("test (all)", scores, yt, wt, high, low))
    for label, keep in (("  multirepo", "mr"), ("  pixel set", "px")):
        mask = np.array([r["set"] == keep for r in test])
        print(report("test" + label, scores[mask], yt[mask], wt[mask], high, low))
    for repo in ("pixel", "excalidraw", "requests", "ripgrep"):
        mask = np.array([r["set"] == "mr" and r["repo"] == repo for r in test])
        if mask.any():
            print(report("test  multirepo " + repo, scores[mask], yt[mask], wt[mask], high, low))
    print("--- the first gate (fitted on Pixel alone), same rows, from the logged tier ---")
    for split_name, rows_ in (("dev", dev), ("test", test)):
        for label, keep in (("multirepo", "mr"), ("pixel set", "px")):
            picked = [r for r in rows_ if r["set"] == keep]
            ys = np.array([int(r["on_topic"]) for r in picked])
            ws = weights(picked)
            hi = np.array([r["tier"] == "high" for r in picked], float)
            lo = np.array([r["tier"] in ("high", "low") for r in picked], float)
            print(f"{split_name + ' ' + label:34} high TPR {np.sum(ws[ys == 1] * hi[ys == 1]) / ws[ys == 1].sum():.2f} "
                  f"FPR {np.sum(ws[ys == 0] * hi[ys == 0]) / ws[ys == 0].sum():.2f}  >=low TPR "
                  f"{np.sum(ws[ys == 1] * lo[ys == 1]) / ws[ys == 1].sum():.2f} "
                  f"FPR {np.sum(ws[ys == 0] * lo[ys == 0]) / ws[ys == 0].sum():.2f}")
    summary = {"names": names, "intercept_raw": intercept, "raw_coefficients": coefficients, "high": high, "low": low,
               "scaler_mean": [float(m) for m in scaler.mean_], "scaler_scale": [float(s) for s in scaler.scale_]}
    if "--template" in argv:
        template = json.load(open(argv[argv.index("--template") + 1]))
        summary = export(template, summary, model, dev, test, oof, y, w, scores, yt, wt)
    json.dump(summary, open(argv[1], "w"), indent=1)


COVERAGE = {
    "definition": "shared / informative, 0 when informative is 0. informative = the keyword rows with a weight above zero "
                  "(row_weight as in struct_ratio; 0 for a French-only word the repository lacks); shared = how many of them "
                  "the heaviest co-file (the first of equals) holds in its keywords",
    "sources": ["facts.relevance.keywords[]", "facts.relevance.cofiles[].keywords", "facts.relevance.cofiles[].weight"],
    "why": "how much of what the prompt says one file covers; it does not depend on the size of the repository",
}


def export(template, summary, model, dev, test, oof, y, w, scores, yt, wt):
    """A full gate-model.json: the template's constants and edge cases with the new fit."""
    out = json.loads(json.dumps(template))
    names = summary["names"]
    by_name = {f["name"]: f for f in template["features"]}
    by_name["coverage"] = dict(COVERAGE, name="coverage")
    features = []
    for index, name in enumerate(names):
        feature = dict(by_name[name])
        feature.pop("mean", None)
        feature.pop("std", None)
        feature.pop("coefficient", None)
        feature["mean"] = summary["scaler_mean"][index]
        feature["std"] = summary["scaler_scale"][index]
        feature["coefficient"] = float(model.coef_[0][index])
        feature["raw_coefficient"] = summary["raw_coefficients"][index]
        features.append(feature)
    out["name"] = "multi-repository compact gate, five features"
    out["scope"] = ("English prompts of four repositories (pixel, excalidraw, requests, ripgrep); French rows were not "
                    "used and still score off")
    out["features"] = features
    out["intercept_raw"] = summary["intercept_raw"]
    out["intercept_standardised"] = float(model.intercept_[0])
    high, low = summary["high"], summary["low"]
    for tier, cut, fpr in (("high", high, 0.05), ("low", low, 0.10)):
        t, f = rates(oof, y, cut, w)
        out["thresholds"][tier] = {
            "score": cut, "target_fpr": fpr, "rule": template["thresholds"][tier]["rule"],
            "out_of_fold": {"tpr": t, "fpr": f},
            "test": dict(zip(("tpr", "fpr"), rates(scores, yt, cut, wt))),
        }
    out["thresholds"]["calibration"] = (
        "the 5 % and 10 % points of the pooled out-of-fold scores (20 repeats of 5-fold, grouped by prompt text) of the "
        "off-topic English dev rows of both sets; an off-topic multirepo prompt is scored against four repositories and "
        "counts once (weight 0.25 per row), in the fit too; a prompt is above a threshold when its score is strictly greater")
    out["fit"] = {
        "sets": ["eval/brief-gate/prompts-multirepo.jsonl", "eval/brief-gate/prompts.jsonl (English rows)"],
        "features_dump": "scripts/bench/research-gate/results/multirepo-features.json (the gate's own features, as `pixel brief --json` logs them)",
        "split": "dev",
        "rows": {"dev_on_topic": int(y.sum()), "dev_off_topic_weight": float(w[y == 0].sum()),
                 "test_on_topic": int(yt.sum()), "test_off_topic_weight": float(wt[yt == 0].sum())},
        "estimator": "L2 logistic regression, C=0.3, balanced classes by weight, features standardised on the fit rows, lbfgs",
        "validation": "StratifiedGroupKFold 5-fold repeated 20 times, grouped by prompt text",
        "commands": [
            "python3 scripts/bench/research-gate/multirepo_set.py <manifest.json> eval/brief-gate/prompts-multirepo.jsonl",
            "pixel brief --json <prompt> <repo at its pinned sha>   # per (prompt, repo); see results/multirepo-fit.txt",
            "python3 scripts/bench/research-gate/gate_multirepo.py scripts/bench/research-gate/results/multirepo-features.json "
            "scripts/bench/research-gate/results/gate-model.json --template scripts/bench/research-gate/results/gate-model-pixel-only.json",
        ],
    }
    out["metrics"] = {
        "dev_out_of_fold": {"auc": auc(oof, y, w), "high": dict(zip(("tpr", "fpr"), rates(oof, y, high, w))),
                            "low": dict(zip(("tpr", "fpr"), rates(oof, y, low, w)))},
        "test_read_once": {"auc": auc(scores, yt, wt), "high": dict(zip(("tpr", "fpr"), rates(scores, yt, high, wt))),
                           "low": dict(zip(("tpr", "fpr"), rates(scores, yt, low, wt)))},
    }
    out.pop("information", None)
    return out


if __name__ == "__main__":
    main(sys.argv[1:])
