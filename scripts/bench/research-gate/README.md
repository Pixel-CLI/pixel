# The brief gate's model (research, #883)

`results/gate-model.json` is the model the gate of PR 3 embeds as constants: four features, a
logistic score and two thresholds, fitted on the English prompts of the `dev` split of
`eval/brief-gate/prompts.jsonl` (PR #884). Everything here reproduces it; nothing here runs in
`pixel`.

| tier | rule | on the English dev rows (out-of-fold) |
| --- | --- | --- |
| high: full brief | score > 1.298 | TPR 0.605 at FPR 0.029 (1 of 35 off-topic prompts) |
| low: compact "possibly related" | 1.298 >= score > 0.731 | TPR 0.721 at FPR 0.086 (3 of 35) |

Repeated 5-fold (20x): AUC 0.922, TPR at 5 % FP 0.588, at 10 % FP 0.706. Leave-one-source-out: AUC 0.934,
0.651, 0.698. 78 English rows (43 on-topic, 35 off-topic), so 5 % FP is one prompt: read the figures to
about ±0.1. French dev rows (31) were not used and all score off (information only). The test split was
never read.

## The features

All four come from the `facts.relevance` block and the typed prompt; the exact definitions, means, standard
deviations and coefficients are in the JSON, and `gate_reference.py` is the executable definition (it
reads only a block, a prompt and the JSON).

| feature | definition (short) |
| --- | --- |
| `struct_per_mille` | ln(1 + 1000 x `structural_files` / `files_considered`) |
| `question` | prompt ends with `?` or one of its first three words is a question word |
| `ops_share` | share of the `keywords` rows that are git/release/CI words |
| `struct_ratio` | best structural `cofiles[].weight` / sum of `row_weight` over `keywords` |

`structural_files` is the one field this work added to the block. The chunk fields of the `meaning` op
(best chunk BM25, best chunk co-occurrence) were tried as the (b) variant and did not beat the block-only
model (mean of CV and LOSO TPR@10: 0.65 to 0.69 against 0.70), so the `meaning` op needs no new field.
No exemplars (kNN), no LLM, no French feature.

## Reproduce

The fitting scripts (`gate_compact.py`, `gate_export.py`, `gate_forward.py`, `gate_pick_compact.py`)
need numpy and scikit-learn; `gate_reference.py` and `make_input.py` run on the standard library.

```bash
# 1. the fixture the prompts were labelled against, indexed
git worktree add --detach /tmp/relevance-fixture 85bede7d9a3c4e385e0a2045241a6466efbd8cbd
pixel prepare-repo /tmp/relevance-fixture
# 2. the block of every dev prompt, from this tree (fit commit in the JSON)
git show origin/feat/883-brief-eval:eval/brief-gate/prompts.jsonl > /tmp/prompts.jsonl   # sha256 in the JSON
PIXEL_WEIGHT_FIXTURE=/tmp/relevance-fixture PIXEL_WEIGHT_PROMPTS=/tmp/prompts.jsonl PIXEL_WEIGHT_SPLIT=dev \
  PIXEL_WEIGHT_OUT=/tmp/relevance-dump-dev5.json \
  cargo test -p pixel-daemon dump_the_raw_material -- --ignored
# 3. fit, calibrate on out-of-fold scores, export (cross-checks the reference scorer on the way)
GATE_DUMP=/tmp/relevance-dump-dev5.json python3 scripts/bench/research-gate/gate_export.py \
  /tmp/relevance-dump-dev5.json /tmp/prompts.jsonl scripts/bench/research-gate/results/gate-model.json
# 4. score one block
python3 scripts/bench/research-gate/gate_reference.py scripts/bench/research-gate/results/gate-model.json block.json "how does install handle existing settings"
```

`gate_export.py` expects the fixture checkout beside the dump (`<dump dir>/relevance-fixture`) for the
exploration cross-check. The model search is `gate_compact.py --experiment`, `gate_forward.py` and
`gate_pick_compact.py` (output in `results/compact-models.txt`); the chunk-field experiment needs the
resident index of PR #886 (`gate_features.rs`, `trace_method.rs.txt`, `make_input.py`).

## Not in the tree

The features and models were designed on dev only. A threshold is a point of the out-of-fold scores of 35
off-topic prompts; refit it when the prompt set grows, and read `test` once, after the choice.

## Multi-repository refit (#906)

The first model was fitted on prompts about Pixel alone and closed 7 of the 13 answer prompts of the tool census
(excalidraw, requests, ripgrep, pixel) and 13 of the 19 prompts the gate sees (the 20th names a symbol and skips it). `results/gate-model.json` is now the refit on
`eval/brief-gate/prompts-multirepo.jsonl` plus the English rows of `prompts.jsonl`; the Pixel-only model is kept as
`results/gate-model-pixel-only.json`.

| | first gate (Pixel only) | this gate |
| --- | --- | --- |
| features | struct_per_mille, question, ops_share, struct_ratio | + `coverage` (shared / informative keywords) |
| HIGH / LOW | 1.298 / 0.731 | 1.184 / 0.612 |
| multirepo test, high tier: TPR / FPR | 0.52 / 0.00 | 0.58 / 0.00 |
| multirepo test, high or low: TPR / FPR | 0.63 / 0.00 | 0.73 / 0.00 |
| Pixel set test, high tier | 0.62 / 0.12 | 0.65 / 0.12 |
| Pixel set test, high or low | 0.74 / 0.19 | 0.85 / 0.15 |
| Pixel set dev (out of fold), high / high or low | 0.64 / 0.03, 0.70 / 0.10 | 0.67 / 0.07, 0.85 / 0.10 |

`results/multirepo-fit.txt` is the full output. Rates weigh an off-topic multirepo prompt once (it is scored
against four repositories). `scripts/bench/research-gate/multirepo_set.py` writes the set; `results/multirepo-features.json`
holds the gate's own features per (prompt, repository) as `pixel brief --json` logs them (harvested with the
first gate's binary at the pinned commits, indexed with `pixel prepare-repo`), so the fit needs no cargo dump:

```bash
python3 scripts/bench/research-gate/gate_multirepo.py scripts/bench/research-gate/results/multirepo-features.json \
  scripts/bench/research-gate/results/gate-model.json --template scripts/bench/research-gate/results/gate-model-pixel-only.json
```

What was tried on dev only (grouped repeated 5-fold, then train-on-one-set/test-on-the-other): widening
`question` to imperatives and first-person openers lowered out-of-fold TPR (off-topic prompts open the same way);
`log(files_considered)` and the prompt's word count raised it inside the pooled dev rows but did not transfer between
the Pixel set and the multirepo set, so neither is in the model; `coverage` helped both ways. The model without
`coverage` was also scored on `test` as an ablation (high tier 0.64 / 0.07, high or low 0.72 / 0.07 on all test rows)
after the choice was made on dev; it did not decide anything. Of those 19 census prompts, 6 opened under the first gate,
9 under this one (pixel-5, excalidraw-1, excalidraw-3, ripgrep-3 gained; ripgrep-5 fell to off, 0.76 to 0.43).
The overview, review and flow prompts (pixel-3, requests-3, excalidraw-4, excalidraw-5) stay closed: their
words are not the repository's words.
