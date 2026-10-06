classify: add verified-history retrieval tier with offline go/no-go evaluation

Add `pixel classify-eval` for offline evaluation of the verified-history
retrieval tier against frozen baselines, and `pixel classify-history` for
managing the store. The tier answers from human-verified history before
model fallback, with abstention on empty history, rubric mismatch,
insufficient support, or novelty. Model predictions cannot self-certify
into gold labels.
