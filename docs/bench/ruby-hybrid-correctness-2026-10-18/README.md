Scripts, samples and verdicts of `../ruby-hybrid-correctness-2026-10-18.md`.

- `build.sh PIXEL V1_CLONE HYBRID_CLONE HOMES`: both graphs of one corpus,
  `rebuild-graph` with `PIXEL_GRAPH_SOURCE_RUBY` unset and `hybrid`, each
  under the empty `HOME` of `../ruby-hybrid-daemon-2026-10-18/env.sh`.
- `compare.py`: the comparison (standard library, `python3 -I`). Writes
  `runs/<corpus>/summary.json`, the seeded samples `sample-*.tsv`, and the
  deltas `delta-*.tsv` (regenerated, not committed: `.gitignore`); prints one
  line per category, per case of pixel-mvp#111 and a verdict; exits 1 while
  a case is open, a judged edge the merge wrote is wrong, or a sampled row
  lacks a verdict.
- `known.py`: the verdicts pixel-research-mvp already gave to gumroad edges
  (`known.tsv`, from its `evaluation/results/2026-10-1*` folders at
  `cf6ebd7`), by edge identity (caller path, line, callee path, callee,
  callee line); a sampled edge with one carries it as `reused_verdict`.
- `v1-sample.tsv`, `judgments/v1-{a,b}.tsv`: v1's level, the 80-edge random
  sample of v1's gumroad call edges and its verdicts (pixel-research-mvp
  `2026-10-15-ruby-graph-vs-v1`, unchanged).
- `JUDGING.md`: the judging guide (pixel-research-mvp
  `2026-10-15-ruby-hybrid-in-v1/JUDGING.md`, unchanged).
- `judgments/sample-*.tsv`: this run's verdicts (`id<TAB>verdict<TAB>reason`),
  one agent session per file, rows without a reused verdict only.

Sampling: `random.Random("20261018-correctness-<side>").sample(rows, n)` over
the delta side sorted by (caller path, line, callee, callee path, callee
line); a side with no more than `n` rows is taken whole. Calls with and
without a receiver are sampled together, as v1's level was.
