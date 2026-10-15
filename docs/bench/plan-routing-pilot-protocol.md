# Frozen pilot: bounded PlanQuery routing

Status: preregistered before candidate fitting or test predictions. Source base:
`5c894b1b3a5db713615d906dc8604805c73813ae`; dedicated branch
`experiment/coding-routing-pilot`. No product defaults change.

## Decision and target selection

Input: an English coding task string. Output: an unordered set of the five
existing PlanQuery **names**. Tag filters, limits and concept-text extraction
are outside this pilot. No replacement of caller-defined `pixel classify`
labels/criteria is implied. Routing is advisory retrieval, never authorization
or evidence of an absent call edge.

Ranking remains the preferred product target, but the available 41-case corpus
in `crates/pixel/tests/cli/scope_task_precision.rs` uses changed Rust files as
labels, is all Pixel history, and already selected ranking fixes/floors. It
cannot honestly supply independently adjudicated negatives or a new locked
repository-held-out test. This pilot therefore finishes only PlanQuery routing.
A fresh independent audit will check this rationale before execution.

Operational gold: dead-interactive finds JSX controls missing handlers;
dead-code finds zero-caller functions/methods; hotspots ranks files by fan-in;
recent-changes retrieves recent history; by-concept locates a named area when
none of those broad scans is requested. A bug is not by itself a request for
history, a local refactor is not by itself a request for global fan-in, and
removing a runtime value is not a request for uncalled functions. Negated or
merely quoted requests do not activate a query. For this experiment by-concept
is exclusive fallback, matching the existing routing shape. Gold is a judgment
of useful retrieval under this policy, not an executable proof of task success.

## Hypothesis and frozen candidates

Hypothesis: sparse contextual word/character features improve exact query-set
selection relative to isolated trigger words at low local cost.

Reference: actual daemon `Plan` request query names, deduplicated, with literal
JSON responses retained. Pre-test API inspection found that `pixel plan --json`
only renders findings, hiding query names; use its underlying NDJSON daemon API
instead. A faithful Python transcription permits resident-only baseline timing;
parity against the actual daemon is required on every corpus row.

One candidate, no neural weights: sklearn TF-IDF FeatureUnion of lowercase word
1-2 grams and character-within-word 3-5 grams, sublinear_tf=True, min_df=1,
L2 normalization per vectorizer, no stopword removal, combined sparse features.
One-vs-rest LogisticRegression(C=1.0, class_weight='balanced', solver='liblinear',
max_iter=1000, random_state=42). Fit four specialized labels. Threshold each at
0.5; emit by-concept only when none passes. No grid, threshold fitting, seed
search, rules added after test, E5 or RLCD. Probabilities are experimental,
uncalibrated binary query scores, not evidence of correctness.

## Corpus and splits

Small explicitly synthetic coding challenge, not observed user traffic and not
JevBench. Parent authors training requests; a separate fresh-context auditor
provides test requests and gold without candidate outputs. Store text, exact
source commit, repository, family, split, label set and rationale. Preserve all
counterfactual/paraphrase family variants in one split. Use no development split
or hyperparameter selection. Check exact normalized duplicates and token/character
similarity across splits; record near pairs rather than claiming automatic
semantic independence. Before predictions, reject or regroup exact duplicates;
any test-family overlap with train invalidates the confirmatory interpretation.
All cases concern Pixel's taxonomy: no held-out repository/fork claim is possible.
Shared intent classes are necessary; phrasing, scenario and contrast families
must be distinct. No unlabeled file is assigned a negative label.

Include quoted/negated intent, noun homonyms, local implementation versus broad
scan, and multiple requested scans. This tests routing only; it does not test
file ranking's implementation/test neighbors or graph utility distractors.
Freeze corpus, script and protocol SHA256 in a manifest before running test.
The implementer must not inspect test text until the candidate is frozen.

## Metrics, acceptance and stop

Primary: exact-set accuracy. Secondary: five-label macro-F1, per-label TP/FP/FN,
weighted error cost per task = (FP + 2*FN), paired wins/losses and a family-cluster
bootstrap 95% interval of the accuracy difference (2,000 draws, seed 42).
Report errors with task IDs. Report Brier score over the four binary specialized
labels and risk (set error) at confidence >= 0.5, 0.7, 0.9 using the minimum
binary decision confidence across labels; no calibration fitting.

A promising experimental result requires >=10 percentage points exact-set gain,
no macro-F1 loss, lower weighted error cost, and no more severe omissions.
Even passing does not authorize integration: synthetic data and no repository
holdout require an independently reviewed real-task corpus. Stop after one
candidate; a negative result is complete. No post-test variants.

## Resources, timing and reproducibility

CPU only, one numerical thread, <=15 minutes and <=4 GiB RSS per fitting run.
Fresh run directory, pinned Python package versions, model serialization/hash,
CLI binary hash, source hash, commands, environment, raw predictions, timings
and errors. Separate import/process startup, serialized-model load, fitting,
resident transformation+prediction and actual CLI timing. Model RSS is process
peak (runtime included). Record machine load; absent an idle-host check (>=95%
idle, no concurrent build/inference, no swap-ins during sample), clocks are
incidental observations, never validated speed evidence. Never stop user jobs.

Run prototype contract tests and `scripts/ci/gates.sh`. No Rust edits planned;
Cargo gates/install/doctor follow the repository's docs/bench-only exemption.
No push, PR or publication.
