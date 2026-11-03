# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT
"""Research: greedy forward selection of a compact gate on the English dev rows (no length, no file size, no chunk fields)."""
import os, sys
import numpy as np
HERE = os.path.dirname(os.path.abspath(__file__))
sys.argv = [sys.argv[0], sys.argv[1]]
src = open(os.path.join(HERE, "gate_compact.py")).read().split("# ---- the experiment")[0]
__file__ = os.path.join(HERE, "gate_compact.py")
exec(compile(src, __file__, "exec"))
table, names, X, y, groups, idx = main.__wrapped__() if hasattr(main, "__wrapped__") else main()
en = np.array([r["lang"] == "en" for r in table])
Xe, ye, ge = X[en], y[en], groups[en]
pool = [n for n in names if not n.startswith("chunk_") and n not in ("length", "struct_lines_norm", "struct_symbols_norm")]
chosen = []
print("\ngreedy forward selection by repeated 5-fold CV AUC (5 repeats), C=0.3")
for step in range(8):
    best = None
    for n in pool:
        if n in chosen:
            continue
        res = evaluate(Xe, ye, ge, [idx[m] for m in chosen + [n]], c=0.3, repeats=5)
        if best is None or res["auc"][0] > best[1]["auc"][0]:
            best = (n, res)
    chosen.append(best[0])
    full = evaluate(Xe, ye, ge, [idx[m] for m in chosen], c=0.3, repeats=20)
    lo = full["loso"]
    print(f"k={step + 1} +{best[0]:20} CV AUC {full['auc'][0]:.3f} TPR@5 {full['tpr5'][0]:.3f} TPR@10 {full['tpr10'][0]:.3f} | LOSO AUC {lo['auc']:.3f} TPR@5 {lo['tpr5']:.3f} TPR@10 {lo['tpr10']:.3f}")
print("order:", chosen)
