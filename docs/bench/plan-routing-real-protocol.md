# Frozen follow-up: real requests and mixed-query routing

2026-09-22. Scope: isolated benchmark/prototype; no product defaults change.
Source HEAD `5c894b1b3a5db713615d906dc8604805c73813ae`; fetched main
`f7ae9d044b1619ca5beeb9d14d0f2a29477500fa` has identical Rust/Cargo sources.
Use the verified source binary from the current-source replay. No new model fit.

## Decision and mixed-query contract

Input is an English coding request, at most 6,000 characters, preserved verbatim.
Output is an ordered list of independent existing PlanQuery jobs. Each job has a
query name, request evidence span, and explicit parameters. Specialized queries
retain existing semantics/caps (unfiltered handlerless JSX, uncalled callables,
10 cross-file-call fan-in hotspots, 20 files ranked by 30-day commit churn).
Concept jobs receive the text of their own requested location clause. A concept
lookup may coexist with a specialized scan; neither filters the other. Requests
such as 'unused functions only in recently changed files' need a relational
intersection the existing independent jobs do not express. Mark such composition
unsupported rather than pretending union implements it. The prototype is advisory
retrieval, not edit authorization or proof that graph edges do not exist.

Quoted literals, code fences and negated operations are not affirmative scan
requests. Unsupported syntax or uncertainty uses concept fallback plus an explicit
warning; no claim of semantic certainty. Explicit unsupported intersections return
an unsupported status and no executable jobs. Input/candidate caps and failures
are surfaced, never silently truncated. No arbitrary-label `classify` replacement.

## Corpus, selection and independent gold

Real public GitHub issue title + unmodified body, no PRs, no rewriting by an agent.
Three unrelated repositories: BurntSushi/ripgrep, astral-sh/ruff,
excalidraw/excalidraw. From each repository's newest issue API pages already
retrieved, choose the first 15 issue records with 40..6,000 input characters,
ordered by issue creation descending with issue number as tie break. Do not
filter by labels, query words or model outputs. Keep all exclusions with reasons.
Pixel's available one issue is development-only and excluded from test.
Pin each repository HEAD as contextual snapshot, source URL/issue number,
created/updated timestamps, exact text hash and raw API response. Snapshot HEAD
is collection context, not a claim about the commit that introduced the issue.
Repository/fork metadata and raw captures are retained. All 45 external rows are
locked test: no fitting or selection on them. Original synthetic Pixel data are
now development/mechanism diagnostics only. No JevBench data.

One independent agent labels requests from PlanQuery operational semantics;
a second independently checks all labels and quotes, without candidate predictions.
Parent adjudicates disagreements before evaluation. Gold is a relevance judgment,
not proof of downstream repair. Store labels, family, evidence quote, rationale,
ambiguity, unsupported-composition flag. Preserve issue variants/linked families
in the same split; all external rows are test-only. Check normalized exact and
character/token near duplicates across previous development inputs and current
repositories. Group correlated issue families for uncertainty; do not call a
unique issue ID evidence of semantic independence. Ambiguous labels are retained
as ambiguous and excluded from the primary score before predictions, with counts.

## Hypothesis and candidates (frozen before test gold/predictions)

Hypothesis: requiring an affirmative request for a query's actual operation
reduces unrelated global scans while clause-local concept retrieval handles
mixed requests. Raw issue reports may contain almost no affirmative scan requests;
this is a coverage question, not evidence that always selecting concept is a
complete router. Include that degenerate control explicitly.

Reference: current-source classify_prompt name set, served by actual daemon Plan.
Three candidates maximum, all non-generative:

1. Conservative deterministic clause rules. Split on newlines, semicolons and
   conjunctions introducing explicit request verbs. Ignore fenced/quoted text
   for scan-trigger detection; skip clauses starting a negation or exclusion.
   Specialized query requires both an affirmative request verb and operational
   evidence (handlerless/unwired JSX controls; zero-caller/unreferenced callable
   audit; highest fan-in/distinct calling-file ranking; recent history/churn
   review). Local refactor/remove/bug mentions alone are not scan requests.
   Named-code location clauses add by-concept even when another query is selected.
   No external model, rule fitting or dictionary additions after gold is revealed.
2. Previously frozen word/character TF-IDF + logistic artifact, unchanged threshold
   0.5/exclusive concept fallback. It cannot express the richer mixed-query policy;
   preserve that limitation, no hidden repair or retraining.
3. Always-by-concept control, to expose a fallback-dominated corpus.

Freeze implementation/model/input/gold/protocol SHA256 before test inference.
No variant or threshold sweep after the locked test. Corpus semantics can be
adjudicated before outcomes, never relabeled to improve scores.

## Metrics, acceptance and stop

Primary: exact label-set accuracy; secondary five-label macro-F1, per-label
support/TP/FP/FN, weighted cost FP+2FN per task, and errors with request IDs.
Report each repository separately. Report family-cluster bootstrap 95% deltas
(2,000 resamples, seed 42) with the caveat that three repositories do not estimate
broad cross-repository uncertainty. Unsupported predictions on supported gold
count as misses, not dropped rows. No probability/calibration claim for rules.
Report the frozen linear model's raw binary scores, without treating confidence
as evidence. No dynamic-label neural runtime.

Adoption evidence requires >=5 independently adjudicated test positives per
specialized query, >=5 mixed-query task families, improvement beyond the constant
control, >=10-point exact-set gain over current rules, lower error cost and no
increased specialized omissions. If these coverage conditions fail, finish the
pilot but label it a real-request false-positive diagnostic, not a validated
replacement. Do not augment test with keyword-selected positives after results.

Separately execute a mixed-query mechanism check against actual source daemon
operations: query-specific concept text, both independent outputs retained with
raw envelopes, unchanged limits, explicit errors/unsupported composition. Those
purpose-built cases are software contract tests, not quality benchmark gold.

## Resources and gates

No training or new weights. Existing source binary, local Python, one numerical
thread. Record process/model load separately from resident inference and peak RSS.
Sample host activity; unless >=95% idle with no competing build/inference and no
swap-ins, quality continues but timings remain unvalidated. No latency campaign
on a busy host. Do not stop user processes. New artifact root:
`.pixel/experiments/plan-routing-real-v1/`; preserve all previous runs.

Run meaningful prototype tests, frozen-corpus validation, actual source-daemon
contract checks and scripts/ci/gates.sh. Rust/build/defaults unchanged: no install
loop or local mutants. No push, PR, publication or global configuration changes.
