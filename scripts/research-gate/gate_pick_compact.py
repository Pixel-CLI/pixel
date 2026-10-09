# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT
"""Research: the short list of compact gates, (a) block and prompt only against (b) plus chunk fields."""
import os, sys
import numpy as np
HERE = os.path.dirname(os.path.abspath(__file__))
S = sys.argv[1]
sys.argv = [sys.argv[0], S]
__file__ = os.path.join(HERE, "gate_compact.py")
exec(compile(open(__file__).read().split("# ---- the experiment")[0], __file__, "exec"))
table, names, X, y, groups, idx = main()
en = np.array([r["lang"] == "en" for r in table])
Xe, ye, ge = X[en], y[en], groups[en]
SHORT = {
    "a3 count": ["n_struct_files", "question", "ops_share"],
    "a3 per-mille": ["struct_per_mille", "question", "ops_share"],
    "a4 count": ["n_struct_files", "question", "ops_share", "struct_ratio"],
    "a4 per-mille": ["struct_per_mille", "question", "ops_share", "struct_ratio"],
    "a5 per-mille + n_kw": ["struct_per_mille", "question", "ops_share", "struct_ratio", "n_kw"],
    "a6 per-mille + depth": ["struct_per_mille", "question", "ops_share", "struct_ratio", "cofile_depth"],
    "b3 a3 + chunk 2": ["struct_per_mille", "question", "ops_share", "chunk_nc_top1_raw", "chunk_best_cooc"],
    "b4 a3 + chunk 3": ["struct_per_mille", "question", "ops_share", "chunk_nc_top1_raw", "chunk_best_cooc", "chunk_cooc2_log"],
    "b5 a4 + chunk 3": ["struct_per_mille", "question", "ops_share", "struct_ratio", "chunk_nc_top1_raw", "chunk_best_cooc", "chunk_cooc2_log"],
}
print("\nEnglish rows:", int(en.sum()), "- repeated 5-fold x20 and leave-one-source-out, L2 C=0.3")
print(f"{'model':26} {'CV AUC':>7} {'TPR@5':>6} {'TPR@10':>7} | {'LOSO AUC':>8} {'TPR@5':>6} {'TPR@10':>7} | mean(CV,LOSO) TPR@10")
for label, cols in SHORT.items():
    for c in (0.3,):
        r = evaluate(Xe, ye, ge, [idx[n] for n in cols], c=c)
        lo = r["loso"]
        print(f"{label:26} {r['auc'][0]:7.3f} {r['tpr5'][0]:6.3f} {r['tpr10'][0]:7.3f} | {lo['auc']:8.3f} {lo['tpr5']:6.3f} {lo['tpr10']:7.3f} | {(r['tpr10'][0] + lo['tpr10']) / 2:.3f}")
