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
GATE_DUMP=/tmp/relevance-dump-dev5.json python3 scripts/research-gate/gate_export.py \
  /tmp/relevance-dump-dev5.json /tmp/prompts.jsonl scripts/research-gate/results/gate-model.json
# 4. score one block
python3 scripts/research-gate/gate_reference.py scripts/research-gate/results/gate-model.json block.json "how does install handle existing settings"
```

`gate_export.py` expects the fixture checkout beside the dump (`<dump dir>/relevance-fixture`) for the
exploration cross-check. The model search is `gate_compact.py --experiment`, `gate_forward.py` and
`gate_pick_compact.py` (output in `results/compact-models.txt`); the chunk-field experiment needs the
resident index of PR #886 (`gate_features.rs`, `trace_method.rs.txt`, `make_input.py`).

## Not in the tree

The features and models were designed on dev only. A threshold is a point of the out-of-fold scores of 35
off-topic prompts; refit it when the prompt set grows, and read `test` once, after the choice.
