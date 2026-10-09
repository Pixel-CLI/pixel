# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT
"""Compact gate models: at most ten features, no exemplars, no LLM.

Every feature is defined from the `facts.relevance` block (`keywords` rows, `cofiles`, the counts), the typed
prompt, and, for the (b) models, chunk fields of the `meaning` op. `features()` below is the reference
definition; `gate-model.json` carries the same definitions for the Rust lane.

usage: gate_compact.py <scratch dir with dump/trace/prompts> [--export]
"""
import importlib.util
import json
import math
import os
import re
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
    spec.loader.exec_module(module)
    return module


bench = load_module("bench", "../bench-relevance-weights.py")

CHOSEN = bench.Rule("chosen", common=True, truncation="bound", by_weight=8, structural=4)
ALL = bench.Rule("all", common=True, truncation="bound", by_weight=10**9, structural=0)

OPS_VOCAB = sorted(set(
    "git commit commits branch branches merge merged pull push rebase squash release tag tags stash checkout revert "
    "cherry pr deploy publish version bump changelog ci remote origin upstream fetch clone main master amend force "
    "workflow pipeline conflict".split()
))
QUESTION_WORDS = sorted(set("how what why where which who when does do is are can could should would".split()))
PASTE = re.compile(r"<pasted_content[^>]*>.*?(?:</pasted_content>|$)", re.S)

# name -> (group, one-line definition). The long definitions are in gate-model.json.
FEATURES = {}


def feat(group):
    def register(fn):
        FEATURES[fn.__name__] = (group, fn)
        return fn
    return register


def split_words(text):
    return re.findall(r"[a-z0-9_]+", text.lower())


def weight_of(row, n):
    """pixel_daemon::relevance::row_weight."""
    if row.get("common"):
        return 0.0
    df = max(row["content_files"], row["symbol_files"], row["filename_files"])
    return bench.keyword_weight(df, n, False, bench.Rule("t", truncation="bound"))


def block_inputs(prompt, counts):
    """The pieces of `facts.relevance` a feature may read."""
    n = prompt["files_considered"]
    rows = prompt["rows"]
    weights = [weight_of(r, n) for r in rows]
    return dict(n=n, rows=rows, weights=weights, cofiles=prompt["cofiles"], matching=counts[0], structural=counts[1],
                text=prompt["visible_text"], lang=prompt["lang"], graph=prompt["graph"])


# ---- F6: the typed prompt (tokenize_task keywords, raw text) -------------------------------------------------------
@feat("prompt")
def n_kw(b):
    return float(len(b["rows"]))


@feat("prompt")
def question(b):
    words = split_words(b["text"])
    return float(b["text"].strip().endswith("?") or any(w in QUESTION_WORDS for w in words[:3]))


@feat("prompt")
def ops_share(b):
    rows = b["rows"]
    return sum(r["keyword"] in OPS_VOCAB for r in rows) / len(rows) if rows else 0.0


@feat("prompt")
def common_share(b):
    rows = b["rows"]
    return sum(bool(r.get("common")) for r in rows) / len(rows) if rows else 1.0


@feat("prompt")
def n_noncommon(b):
    return float(sum(not r.get("common") for r in b["rows"]))


@feat("prompt")
def length(b):
    return math.log(1 + len(b["text"]))


# ---- F1 / F3: the block's rows and co-files ---------------------------------------------------------------------------
def q_total(b):
    return sum(b["weights"])


def best_structural(b):
    return max((c["weight"] for c in b["cofiles"] if c["structural"]), default=0.0)


@feat("block")
def struct_ratio(b):
    q = q_total(b)
    return best_structural(b) / q if q > 0 else 0.0


@feat("block")
def best_struct(b):
    return best_structural(b)


@feat("block")
def best_any_ratio(b):
    q = q_total(b)
    return (b["cofiles"][0]["weight"] if b["cofiles"] else 0.0) / q if q > 0 else 0.0


@feat("block")
def q_weight(b):
    return q_total(b)


@feat("block")
def n_struct_files(b):
    return math.log1p(b["structural"])


@feat("block")
def n_matching_files(b):
    return math.log1p(b["matching"])


@feat("block")
def struct_per_mille(b):
    """ln(1 + 1000 * structural matching files / files considered): the same ranking as the count, the same value in any repository size."""
    return math.log1p(1000.0 * b["structural"] / b["n"]) if b["n"] else 0.0


@feat("block")
def matching_per_mille(b):
    return math.log1p(1000.0 * b["matching"] / b["n"]) if b["n"] else 0.0


@feat("block")
def cofile_depth(b):
    return float(max((sum(1 for k in c["keywords"] if k in b["noncommon_set"]) for c in b["cofiles"]), default=0))


@feat("block")
def cofile_depth_frac(b):
    nc = len(b["noncommon_set"])
    return cofile_depth(b) / nc if nc else 0.0


@feat("block")
def cofiles_ge2(b):
    return float(sum(1 for c in b["cofiles"] if sum(1 for k in c["keywords"] if k in b["noncommon_set"]) >= 2))


@feat("block")
def cov_df(b):
    nc = [r for r in b["rows"] if not r.get("common")]
    if not nc:
        return 0.0
    return sum(max(r["content_files"], r["symbol_files"], r["filename_files"]) > 0 for r in nc) / len(nc)


@feat("block")
def n_struct_kw(b):
    return float(sum(1 for r in b["rows"] if not r.get("common") and (r["symbol_files"] or r["filename_files"])))


@feat("block")
def max_idf_struct(b):
    return max((w for r, w in zip(b["rows"], b["weights"]) if r["symbol_files"] or r["filename_files"]), default=0.0)


@feat("block")
def max_idf(b):
    return max((w for r, w in zip(b["rows"], b["weights"]) if max(r["content_files"], r["symbol_files"], r["filename_files"]) > 0), default=0.0)


# ---- F2: file size of the listed co-files (needs CoFile.lines / CoFile.symbols) ------------------------------------------
@feat("size")
def struct_lines_norm(b):
    return max((c["weight"] / math.log(2 + c["lines"]) for c in b["cofiles"] if c["structural"]), default=0.0)


@feat("size")
def struct_symbols_norm(b):
    return max((c["weight"] / math.log(2 + c["symbols"]) for c in b["cofiles"] if c["structural"]), default=0.0)


# ---- F4: chunk fields of the meaning op (trace) ---------------------------------------------------------------------------------
CHUNK = ["chunk_nc_top1_raw", "chunk_best_cooc", "chunk_all_margin", "chunk_cooc2_log", "chunk_nc_top1_norm", "chunk_cooc_frac"]


def chunk_features(trace):
    if not trace or not trace["noncommon_terms"]:
        return {name: 0.0 for name in CHUNK}
    top, top_all = trace["bm25_nc_top"], trace["bm25_all_top"]
    ceiling_nc, ceiling_all = trace["ceiling_nc"] or 1.0, trace["ceiling_all"] or 1.0
    return {
        "chunk_nc_top1_raw": top[0][0] if top else 0.0,
        "chunk_best_cooc": float(trace["best_cooc"]),
        "chunk_all_margin": (top_all[0][0] - top_all[-1][0]) / ceiling_all if top_all else 0.0,
        "chunk_cooc2_log": math.log1p(trace["cooc_chunks"][1]),
        "chunk_nc_top1_norm": top[0][0] / ceiling_nc if top else 0.0,
        "chunk_cooc_frac": trace["best_cooc"] / len(trace["noncommon_terms"]),
    }


def build_table(dump, traces, symbols, lines):
    """One row per dev prompt, every feature, from the block-equivalent inputs only."""
    n = dump["files_considered"]
    paths = dump["paths"]
    table = []
    for p in dump["prompts"]:
        row = p["row"]
        text = row["text"]
        tagged = "<pasted_content" in text
        visible = PASTE.sub("", text)
        kws = [] if tagged else p["keywords"]
        rows = [{k: v for k, v in kw.items() if k in ("keyword", "content_files", "symbol_files", "filename_files", "truncated", "common")}
                for kw in ({"keyword": k["keyword"], "content_files": len(k["content"]), "symbol_files": len(k["symbol"]),
                            "filename_files": len(k["filename"]), "truncated": k["truncated"], "common": k["common"]} for k in kws)]
        if kws:
            _, listed = bench.rank(p, n, paths, CHOSEN, None)
            _, every = bench.rank(p, n, paths, ALL, None)
            keywords_of = {}
            # keywords per co-file, as in the block: the rows' keywords that match the file, in task order
            channels = {}
            for i, k in enumerate(kws):
                for kind, ids in (("content", k["content"]), ("symbol", k["symbol"]), ("path", k["filename"])):
                    for pid in ids:
                        channels.setdefault(paths[pid], set()).add(i)
            cofiles = [{"path": pth, "weight": w, "structural": s, "keywords": [kws[i]["keyword"] for i in sorted(channels.get(pth, ()))],
                        "lines": lines.get(pth, 1), "symbols": symbols.get(pth, 0)} for pth, w, s in listed]
        else:
            listed, every, cofiles = [], [], []
        counts = (len(every), sum(1 for _, _, s in every if s))
        prompt = {"files_considered": n, "rows": rows, "cofiles": cofiles, "visible_text": visible, "lang": row["lang"], "graph": True}
        b = block_inputs(prompt, counts)
        b["noncommon_set"] = {r["keyword"] for r in rows if not r.get("common")}
        values = {name: fn(b) for name, (_, fn) in FEATURES.items()}
        values.update(chunk_features(traces.get(row["id"], {}).get("trace") if not tagged else None))
        table.append({"id": row["id"], "text": text, "on_topic": bool(row["on_topic"]), "source": row["source"], "kind": row["kind"],
                      "lang": row["lang"], "values": values, "top": listed[0][0] if listed else None})
    return table


# ---- models -----------------------------------------------------------------------------------------------------------------------
def auc(pos, neg):
    return sum((a > b) + 0.5 * (a == b) for a in pos for b in neg) / (len(pos) * len(neg))


def tpr_at(scores, y, fp):
    neg = np.sort(scores[y == 0])[::-1]
    return float((scores[y == 1] > neg[int(fp * len(neg))]).mean())


def metrics(scores, y):
    return {"auc": auc(scores[y == 1], scores[y == 0]), "tpr5": tpr_at(scores, y, 0.05), "tpr10": tpr_at(scores, y, 0.10)}


def fit_lr(x, y, c):
    scaler = StandardScaler().fit(x)
    model = LogisticRegression(C=c, penalty="l2", class_weight="balanced", max_iter=3000).fit(scaler.transform(x), y)
    return scaler, model


def select_filter(x, y, k, names):
    """Top-k by univariate AUC distance from 0.5 on this (training) data, skipping a feature correlated >0.85 with a kept one."""
    order = sorted(range(x.shape[1]), key=lambda i: -abs(auc(x[y == 1, i], x[y == 0, i]) - 0.5))
    kept = []
    for i in order:
        if all(abs(np.corrcoef(x[:, i], x[:, j])[0, 1]) < 0.85 for j in kept):
            kept.append(i)
        if len(kept) == k:
            break
    return kept


def evaluate(X, y, groups, cols, c=0.3, repeats=20, k=None, names=None):
    """Repeated stratified 5-fold and leave-one-source-out. With k, the columns are chosen inside every training fold from `cols`."""
    def run(tr, te):
        use = cols
        if k:
            use = [cols[i] for i in select_filter(X[tr][:, cols], y[tr], k, names)]
        scaler, model = fit_lr(X[tr][:, use], y[tr], c)
        return model.decision_function(scaler.transform(X[te][:, use]))
    oof = np.zeros((repeats, len(y)))
    for n, (tr, te) in enumerate(RepeatedStratifiedKFold(n_splits=5, n_repeats=repeats, random_state=7).split(X, y)):
        oof[n // 5, te] = run(tr, te)
    cv = [metrics(oof[i], y) for i in range(repeats)]
    lo = np.zeros(len(y))
    for tr, te in LeaveOneGroupOut().split(X, y, groups):
        lo[te] = run(tr, te)
    loso = metrics(lo, y)
    return {key: (float(np.mean([m[key] for m in cv])), float(np.std([m[key] for m in cv]))) for key in ("auc", "tpr5", "tpr10")} | \
           {"loso": loso, "oof": oof.mean(axis=0), "loso_scores": lo}


def load(path):
    with open(path) as handle:
        return json.load(handle)


def file_stats(graph_path, fixture, paths):
    """Symbols per file (from the fixture's graph) and lines per file (from the checkout)."""
    import sqlite3
    db = sqlite3.connect(f"file:{graph_path}?mode=ro", uri=True)
    symbols = dict(db.execute("select f.path, count(s.id) from files f left join symbols s on s.file_id=f.id group by f.id"))
    lines = {}
    for path in paths:
        try:
            with open(os.path.join(fixture, path), "rb") as handle:
                lines[path] = handle.read().count(b"\n") + 1
        except OSError:
            lines[path] = 1
    return symbols, lines


def main():
    S = sys.argv[1]
    dump = load(os.environ.get("GATE_DUMP", f"{S}/relevance-dump-dev5.json"))
    trace_path = f"{S}/relevance-gate-trace.json"
    traces = {t["id"]: t for t in load(trace_path)} if os.path.exists(trace_path) else {}
    symbols, lines = file_stats(f"{S}/relevance-fixture/.pixel/graph.v2.db", f"{S}/relevance-fixture", dump["paths"])
    table = build_table(dump, traces, symbols, lines)
    names = list(table[0]["values"])
    X = np.array([[r["values"][n] for n in names] for r in table], float)
    y = np.array([int(r["on_topic"]) for r in table])
    groups = np.array([r["source"] for r in table])
    idx = {n: i for i, n in enumerate(names)}
    print(f"{len(table)} dev prompts, {int(y.sum())} on-topic, {int((1 - y).sum())} off-topic")
    print("\nsingle-feature AUC:")
    for n in sorted(names, key=lambda n: -abs(auc(X[y == 1, idx[n]], X[y == 0, idx[n]]) - 0.5)):
        print(f"  {n:22} {auc(X[y == 1, idx[n]], X[y == 0, idx[n]]):.3f}")
    return table, names, X, y, groups, idx


if __name__ == "__main__" and "--experiment" not in sys.argv:
    main()


# ---- the experiment (English dev rows only) ------------------------------------------------------------------------------
PROMPT_NO_LEN = ["n_kw", "question", "ops_share"]
SETS = {
    # (a) no chunk index
    "a1 with length": ["n_kw", "question", "ops_share", "length", "struct_ratio", "n_struct_files", "n_matching_files",
                       "cofile_depth", "n_struct_kw", "struct_lines_norm"],
    "a2 no length": ["n_kw", "question", "ops_share", "struct_ratio", "n_struct_files", "n_matching_files",
                     "cofile_depth", "n_struct_kw", "max_idf_struct", "struct_lines_norm"],
    "a3 no length, no file size": ["n_kw", "question", "ops_share", "struct_ratio", "n_struct_files", "n_matching_files",
                                   "cofile_depth", "n_struct_kw", "max_idf_struct", "cov_df"],
    "a4 six features": ["n_kw", "question", "ops_share", "struct_ratio", "n_struct_files", "cofile_depth"],
    # (b) plus chunk fields from the meaning op
    "b1 a2 - 3 + chunk top3": ["n_kw", "question", "ops_share", "struct_ratio", "n_struct_files", "cofile_depth", "n_struct_kw",
                               "chunk_nc_top1_raw", "chunk_best_cooc", "chunk_all_margin"],
    "b2 a3 - 2 + chunk 2": ["n_kw", "question", "ops_share", "struct_ratio", "n_struct_files", "cofile_depth", "n_matching_files",
                            "n_struct_kw", "chunk_nc_top1_raw", "chunk_best_cooc"],
    "b3 a4 + chunk 3": ["n_kw", "question", "ops_share", "struct_ratio", "n_struct_files", "cofile_depth",
                        "chunk_nc_top1_raw", "chunk_best_cooc", "chunk_all_margin"],
}


def show(label, res):
    lo = res["loso"]
    print(f"{label:34} CV AUC {res['auc'][0]:.3f}±{res['auc'][1]:.3f} TPR@5 {res['tpr5'][0]:.3f}±{res['tpr5'][1]:.3f} TPR@10 {res['tpr10'][0]:.3f}±{res['tpr10'][1]:.3f}"
          f" | LOSO AUC {lo['auc']:.3f} TPR@5 {lo['tpr5']:.3f} TPR@10 {lo['tpr10']:.3f}")


def experiment(S):
    table, names, X, y, groups, idx = main()
    en = np.array([r["lang"] == "en" for r in table])
    print(f"\nEnglish rows: {int(en.sum())} ({int(y[en].sum())} on-topic, {int((1 - y[en]).sum())} off-topic); French rows set aside: {int((~en).sum())}")
    Xe, ye, ge = X[en], y[en], groups[en]
    print("\nsingle-feature AUC on English rows:")
    for n in sorted(names, key=lambda n: -abs(auc(Xe[ye == 1, idx[n]], Xe[ye == 0, idx[n]]) - 0.5)):
        print(f"  {n:22} {auc(Xe[ye == 1, idx[n]], Xe[ye == 0, idx[n]]):.3f}")
    print("\nfixed sets, L2 logistic regression, C=0.3 (and 0.1):")
    for label, cols in SETS.items():
        for c in (0.1, 0.3):
            show(f"{label} C={c} ({len(cols)} f)", evaluate(Xe, ye, ge, [idx[n] for n in cols], c=c))
    print("\nfeature selection inside every training fold (top-k by univariate AUC, |r| < 0.85):")
    pools = {
        "pool a (no length)": [n for n in names if not n.startswith("chunk_") and n != "length"],
        "pool a (with length)": [n for n in names if not n.startswith("chunk_")],
        "pool b (no length)": [n for n in names if n != "length"],
    }
    for label, pool in pools.items():
        for k in (6, 8, 10):
            show(f"{label} k={k}", evaluate(Xe, ye, ge, [idx[n] for n in pool], c=0.3, k=k, names=pool))
    return table, names, X, y, groups, idx, en


if __name__ == "__main__" and "--experiment" in sys.argv:
    experiment(sys.argv[1])
