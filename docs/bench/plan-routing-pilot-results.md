# PlanQuery pilot: keep experimental, do not integrate

Follow-up: the [current-source replay](plan-routing-current-source-results.md)
built HEAD explicitly and reproduced all 100 baseline query sets unchanged.
The runtime-version check leaves the experimental-only recommendation unchanged.

Executed 2026-09-22 on `experiment/coding-routing-pilot`, starting from clean
`5c894b1b3a5db713615d906dc8604805c73813ae`. The previous branch and commit
were verified, not reset. Only docs, fixtures and isolated bench scripts changed.
During evaluation, daemon snapshots report six expected untracked experiment
paths (`dirty_count: 6`), not a clean working tree; the base HEAD remained unchanged.

**Recommendation: keep experimental.** Sparse logistic routing improves an
independently authored synthetic diagnostic from 12/40 to 24/40 exact sets,
but both systems score **1/8** on the conservatively isolated novel-combination
slice. Overlapping training/test themes invalidate a broad generalization claim.
No default, `classify` contract, guard, graph proof or command substitution changes.

## Decision and why it matters

Select an unordered set of existing PlanQuery names from an English coding
request. A correct route avoids spending retrieval/context budget on an unrelated
global scan and avoids suppressing useful concept lookup. This measures routing,
not downstream coding success, tag-filter correctness or file relevance.

File ranking was investigated first. The existing pinned-history harness has
41 cases, 65 positive assignments, 41 unique paths, and no independently judged
negatives. Labels are changed Rust files, and these cases already informed
ranking fixes and floors (`scope_task_precision.rs:1-63`). Parent-tree execution
prevents new-byte leakage but does not make unchanged files irrelevant.
Other available qrels are 10 queries/14 files in one crate and five observed
questions/195 files (including near-duplicate installation questions). New
relevance adjudication would be needed for an honest ranking comparison.
The bounded fallback here is therefore **only PlanQuery routing**.

This fixed taxonomy is not a replacement for `pixel classify` with arbitrary
caller labels and criteria. No generative decoder, E5/RLCD download, neural
training, JevBench run or public-answer tuning occurred. Earlier Potion, E5,
Qwen and EttinX results and their scope remain unchanged.

## Corpus, gold and splits

- `scripts/fixtures/plan-routing-train.tsv`: 60 parent-authored synthetic requests,
  30 two-row families. Four specialized binary labels; by-concept fallback.
- `scripts/fixtures/plan-routing-test.jsonl`: 40 requests, 20 two-row families,
  authored separately by the corpus auditor without model outputs or access to
  training rows. Each row records Pixel revision, task, gold, rationale, source
  evidence, provenance and stratum. The candidate was fitted and hashed before
  the parent read this test text. All five existing labels are candidate options.
- No dev split, tuning, threshold search or post-test variant. No exact normalized
  cross-split duplicates; all paired family variants stay together. Nearest-pair
  text similarity is retained in `eval-01/freeze.json` (maximum character
  SequenceMatcher ratio 0.605). Low lexical similarity does not prove independence.
- Gold is a source-grounded judgment of useful query operations, not the current
  keyword output. Parent adjudication and the separate author's rationales are
  agent judgments, not independent human annotations. Rust fixtures support the
  operations' behavior, not proof of successful task completion.

**Important pre-test limitation:** independently written examples still share
semantic themes. The [pre-prediction audit](plan-routing-pilot-gold-audit.md)
quarantines 32 rows from held-out interpretation. F06/F07/F08/F20 (eight rows,
four families) test unseen label combinations; constituent intents remain seen.
All examples use one repository's taxonomy, so there is no repository/fork
holdout or production prevalence claim. The frozen acceptance screen cannot be
used as confirmatory evidence on the overlapping 40-row diagnostic.

Four labels sets (F08/F20 pairs) need by-concept **alongside** a specialized
query. Both frozen output shapes cannot express this; keep these gold sets and
count the errors. This conflicts with the protocol's initial exclusive-fallback
gold assumption, and was recorded before predictions. Maximum exact-set score
for either shape is 36/40 and 4/8 on the novel-combination slice. No option was
silently deleted to improve a result.

The challenge includes quoted/negated requests, linker/button/priority homonyms,
local refactoring versus global fan-in and unrelated temporal keywords. File
ranking distractors such as neighboring tests and connected utilities were not
measured because ranking was not the selected target. Training language sometimes
uses imports/dependencies as shorthand for hotspots; the actual operation counts
distinct calling files. This is additional label/wording noise, not proof of
structural equivalence.

## Frozen comparison and results

Reference: the actual installed Pixel daemon's `Plan` response `result.queries`,
deduplicated. `pixel plan --json` hides query names, so the prototype uses its
underlying NDJSON API and retains complete requests/responses. A source-based
Python name-set transcription matched **100/100** actual responses (60 train,
40 test). Only the test rows entered the scores. The installed daemon's behavior
on these rows is verified; no claim that its entire release equals source HEAD.

Candidate: word TF-IDF 1-2 grams plus char_wb 3-5 grams, sublinear TF, L2 per
vectorizer, 3,641 features; four one-vs-rest logistic regressions, C=1,
balanced classes, liblinear, seed 42, max_iter=1,000, one thread. Each specialized
label uses threshold 0.5; by-concept only if none passes. Eight contract tests
cover scoring errors, multilabel threshold/fallback, invalid probabilities,
baseline token boundaries, duplicate rejection and preserving richer gold.

| 40-row diagnostic | Existing rules | TF-IDF + logistic |
|---|---:|---:|
| Exact set | 12/40 (30%) | 24/40 (60%) |
| Macro-F1, five labels | 0.423208 | 0.744322 |
| False-positive labels | 23 | 8 |
| Omitted labels | 32 | 16 |
| Cost/task: FP + 2 FN | 2.175 | 1.000 |

Paired wins/losses: **15/3**. Family-cluster bootstrap (2,000, seed 42): accuracy
delta +30 points, descriptive 95% interval **[+10, +47.5]** points. This interval
only describes this synthetic challenge; it cannot correct shared-theme leakage,
annotation bias or absent repository diversity.

| Stratum | Rows | Rules exact | Logistic exact |
|---|---:|---:|---:|
| Core single query | 10 | 6 | 10 |
| Multilabel | 10 | 2 | 4 |
| Negation/quotation | 10 | 1 | 6 |
| Homonym/semantic negative | 10 | 3 | 4 |
| Novel combinations, separate subset | 8 | **1** | **1** |

Novel-combination macro-F1: 0.438095 -> 0.718095; cost/task: 3.00 -> 1.75.
That improves partial coverage but does **not** improve the exact-set KPI.
Eight rows/four clusters cannot support a winner selection.

Per-label TP/FP/FN, rules -> logistic:

| Label | Rules | Logistic |
|---|---|---|
| dead-interactive | 5 / 3 / 1 | 6 / 2 / 0 |
| dead-code | 0 / 6 / 6 | 5 / 4 / 1 |
| hotspots | 4 / 3 / 4 | 5 / 1 / 3 |
| recent-changes | 4 / 4 / 4 | 5 / 1 / 3 |
| by-concept | 7 / 7 / 17 | 15 / 0 / 9 |

Important regressions: F07-A loses explicit recent-changes when combined with
hotspots; F15-B mistakes graph edge linking for dead-code; F19-B mistakes deletion
of a deprecated configuration feature for a zero-caller audit. Both F20 requests
lose the requested hotspots query in the candidate. Negated or cited intent
still causes errors (F11-B, F12-A, F18-A). All 16 candidate errors and all 28
reference errors are retained by ID with missing/extra labels in `summary.json`.

Four-label binary Brier: **0.122740**. Set-confidence is the minimum confidence
in the four binary decisions; it is uncalibrated and does not model by-concept
co-occurrence. Risk/coverage at >=0.5: 40/40 covered, 40% set error; >=0.7: 1/40
covered, 0/1 error; >=0.9: 0/40, risk undefined. This offers no useful calibrated
abstention policy and no correctness guarantee. No calibration fitting followed.

## Local resources and clocks

macOS 26.6.2 arm64, Python 3.14.7; package lock committed beside the fixtures.
The host sample was only **78.57% idle**, with about 909 MB unused RAM. No
performance repetition was launched, no user process stopped, and no compilation
was run during evaluation. The following are incidental quality-run clocks,
**not validated latency or comparative speed evidence**.

| Observation | Value |
|---|---:|
| Fit proper | 0.011638 s |
| First fitting process startup/imports | 18.791321 s |
| Complete fit process (`time -l`) | 18.92 s |
| Serialized model/runtime load, evaluation | 0.725390 s |
| Earlier evaluation imports + data validation | 0.332751 s |
| First candidate request, transform + predict | 5.035 ms |
| Remaining 39 candidate requests p50 / p95 | 0.964 / 1.210 ms |
| Python rule transcription p50 / p95 | 0.0209 / 0.0270 ms |
| Actual daemon Plan p50 / p95 | 88.283 / 132.195 ms |
| Serialized model | 189,436 bytes |
| Fit peak RSS (`time -l`) | 122,650,624 bytes (~117 MiB) |
| Evaluation peak RSS (`time -l`) | 123,011,072 bytes (~117 MiB) |

Daemon timing includes graph/history/concept execution, not just routing; the
Python rule port is not a native Rust timing. Do not compare either number to
candidate latency as a product speedup. Load is fresh-process with uncontrolled
filesystem cache, not cold disk. RSS includes Python/sklearn; daemon RSS is not
included in the evaluation process peak. No claim of incremental model-only RSS.
Fit required four solver iterations per label, far inside 15 minutes/4 GiB.

One infrastructure failure preceded fitting: macOS rejected RLIMIT_AS. Its logs
remain intact; a sampled peak-RSS/wall watchdog replaced it before any model was
trained. The sole successful fitting run was not repeated. No inference errors,
parity failures or missing test rows occurred. Evaluation wall time was 10.71 s.

## Reproduce and inspect

Tracked entry points and data:

```sh
R=.pixel/experiments/plan-routing-NEW
python3 -m venv "$R/venv"
"$R/venv/bin/python" -m pip install -r scripts/fixtures/plan-routing-requirements.lock
/usr/bin/time -l "$R/venv/bin/python" scripts/bench/bench-plan-routing.py fit \
  --train scripts/fixtures/plan-routing-train.tsv --output "$R/fit"
pixel daemon status  # use the socket path for this checkout in the next command
/usr/bin/time -l "$R/venv/bin/python" scripts/bench/bench-plan-routing.py evaluate \
  --model "$R/fit" --test scripts/fixtures/plan-routing-test.jsonl \
  --output "$R/eval" --socket /path/reported/by/daemon-status.sock
python3 scripts/test-bench-plan-routing.py
scripts/ci/gates.sh
```

Run directories must be new. Requires an already running indexed Pixel daemon;
no daemon is silently started by the Python script. The scorer fails on resource
limits, malformed gold, duplicate normalized text, missing metadata, changed
model/source hashes, failed RPC or baseline parity mismatch. It does not claim
that family-ID uniqueness proves semantic isolation. Local joblib artifacts
should only be loaded from trusted runs.

Original raw root: `.pixel/experiments/plan-routing-pilot-v1/`. `fit/fit.json`
and `eval-01/freeze.json` store argv/configuration/hashes before predictions;
`eval-01/raw.jsonl` holds all 100 request/response records and 40 candidate
probability vectors; `summary.json`, host snapshots, failure logs, test/gate logs,
original fit-time script/protocol and source-audit reports accompany them.
A durable archive outside ignored repository state is kept at
`~/code/pixel-experiment-artifacts/plan-routing-pilot-v1.tar.gz`.

| Identity | SHA256 |
|---|---|
| Plan Rust source | `f702761ba5141608f311f1832db1f2ef0abbec190ea1e088fe4f857731e1f9a1` |
| Installed Pixel 0.4.0 binary | `b0c3755ba63e35d8188d61a6aac3c788504ff69538d0b36e53b5c10c8de2248d` |
| Training TSV | `8976c4820e0d507508ebf1beb5bdb41239c39e447db0533da2520209f3bd57dd` |
| Test JSONL | `f95f56f1a5be0adca84ca496a68e6f39625ed7188bdf5db2db0af20a6f2aa9c6` |
| Frozen model | `da088ca3b289af9370c0898a23e8aad52d06997f034c64a83a206cc078ebf37c` |
| Fit-time script | `6f9a60ef07ffcdc630461f1338aa1c60f22af3bf249017fd294c511adc30d7ef` |
| Evaluation script | `bc7b6c744eccf0eea02601a570939a2a692e7017c8be72185fce7c7caf012524` |

Before evaluation, AST equality verified unchanged fit, training conversion,
baseline, decoder and scoring functions across fit/evaluation script versions.
The evaluation-only correction admits concept-plus-specialized gold instead of
forcing gold into the candidate's representational limits; no model repair.

The follow-up independent audit reproduced all scores, confusion counts, strata,
bootstrap interval and Brier from raw rows without importing the scorer. It also
verified 60/60 training conversions, 40/40 original-gold conversions and 100/100
baseline parity. One provenance weakness remains: the original gold report and
pre-prediction gold-audit document were not hashed in the pre-evaluation manifest.
The test labels, evaluator and protocol were hashed, but file mtimes/transcript
rather than that manifest establish when the limitation narrative was written.
Final archive hashes cannot retroactively strengthen that pre-test claim. Future
runs should bind those documents before prediction; this pilot was not rerun.

## Gates and retained change

Executed: eight prototype contract tests; Python compilation; 100/100 real-daemon
baseline parity; independent direct recomputation of exact/FP/FN counts;
`scripts/ci/gates.sh` **67 tests (36+12+9+10), exit 0**. Cargo fmt/test/clippy were
explicitly skipped by that runner for docs/bench-only changes. Cargo.lock did
not change: deny not run. No local mutants, Rust build, install/doctor, CI, push,
PR, publication or global configuration change.

Retain the isolated harness, pinned dependency lock, corpus, protocol, gold audit
and evidence. Reject default integration and reject a claim of semantic routing
solved by token n-grams. This experiment identifies two product-relevant gaps:
compositional intent/negation and exclusive concept fallback. A future pilot
would need genuinely independent real-task families and a separately frozen
output-policy decision before any new model candidate. Those are proposals,
not changes made or experiments added after this test.
