# Real-request follow-up: fewer unwanted scans, no validated router replacement

Executed 2026-09-22 on `experiment/coding-routing-pilot`, source HEAD
`5c894b1b3a5db713615d906dc8604805c73813ae`. **Keep experimental; do not change
Pixel's default router or train/download another model on this evidence.**

The isolated mixed-query prototype works in executed contract checks. On 42
unambiguous real public issues, current-source routing chose the proposed gold
set 25 times; conservative rules, the frozen linear model and an always-concept
control each did so 42 times. However, all 42 gold sets contain only by-concept.
This establishes a false-positive diagnostic under the proposed routing policy,
not validation of specialized intent recall, mixed-query quality or a learned
model advantage. The preregistered coverage/adoption gate fails.

## What was implemented and actually exercised

`scripts/bench/bench-plan-mixed.py` is a standalone, non-generative prototype, not wired
into Pixel. It emits ordered independent jobs with original-text evidence spans,
explicit parameters and warnings. A concept-location clause can coexist with a
specialized scan and keeps its own input. Fenced/quoted examples are masked for
scan detection; literal concept targets remain available. Specialized scans
require an affirmative request plus operational terms, rather than a lone
`bug`, `remove`, `button` or `refactor` token. When no supported job is found,
it falls back to concept with a warning. This bounded regex parser is not a
general semantic parser: when another job is recognized, unrecognized clauses
can be missed. Its `supported` status describes executable operations, not a
proof that every requested intent was understood.

The wrapper executes each job as a separate explicit daemon Plan request and
retains each complete response. It does not flatten, cross-filter or merge
findings, discard epistemics/warnings, authorize edits, or infer absent graph
edges. Independent union cannot implement 'unused functions only in recently
changed files'; the prototype rejects that recognized composition as unsupported.
Path-scoped global scans in the supported syntax are also rejected. Input over
6,000 characters or more than eight jobs raises an error instead of truncating.
Limits remain hotspots 10, recent-changes 20 files/30 days, concept matches 20.

Actual source-daemon mechanism check:

| Independent request | Input / limit | Findings observed |
|---|---|---:|
| by-concept | `classify_prompt`, fixed result cap 20 | 1, in `crates/pixel-graph/src/plan.rs` |
| hotspots | limit 10 | 10 |
| recent-changes | limit 20; source's 30-day window | 20 |

The request was `Locate classify_prompt; rank files with highest fan-in; review
recent churn`. Separate raw envelopes, parameters and graph uncertainty were
retained. Unsupported intersection produced no jobs. Software tests additionally
cover duplicate findings retained per job, distinct concept locations, clause
order/spans, exclusions, missing-handler negation, unterminated fences, fixed
caps and error propagation. These purpose-built checks prove those contracts;
they are **not** independent quality benchmark examples.

## Real corpus and gold

Selection was frozen in [the protocol](plan-routing-real-protocol.md) before
candidate/gold outcomes. Inputs are original GitHub issue title + unmodified body,
not agent paraphrases or PR descriptions. Three unrelated non-fork repositories,
15 newest eligible issues each, selected by creation date from retained API pages.
Eligibility: non-PR, 40..6,000 characters. No keyword/model-based selection.
The 500 captured external API records yield 45 selected and 455 exclusions:
419 PRs, two length exclusions, 34 beyond the fixed count. Raw responses and
exclusion receipts remain available. One separately fetched Pixel issue was not
used for fitting, rule design or evaluation.

| Repository | Collection-context commit | Selected / scored |
|---|---|---:|
| BurntSushi/ripgrep | `3fce3b5bb0236da2df6d99672afb8a719642eca7` | 15 / 15 |
| astral-sh/ruff | `d1e47fbf5b8e831795e2b71d2353ae551764cd84` | 15 / 15 |
| excalidraw/excalidraw | `31df3e6ef245c646b18a055ccf858cd6117c7768` | 15 / 12 |

These SHAs identify collection context, not the commit introducing each issue.
Every row stores original URL, issue number, timestamps, verbatim input, input
hash, repository, SHA and provenance. External repositories were not cloned or
indexed: the evaluated observable is repository-independent query selection,
not the returned code's relevance to those issues. Raw baseline Plan calls ran
on the isolated Pixel source worktree; their findings are not external-repository
evidence and are not scored.

A first agent labeled all 45 from PlanQuery operational semantics and literal
issue quotes. A separate fresh-context agent checked every annotation without
candidate code or predictions. It agreed on all label sets but added one
ambiguity flag, accepted before inference. Three Excalidraw rows are excluded
from the primary score: 12137 (apparently another hosted product), 12127 (an
Arabic title plus iframe with unclear coding request), 12103 (hosted Excalidraw+
MCP with uncertain open-source repository location). All 45 predictions remain
in the raw file. No labels or exclusions changed after outcomes.

Gold is an agent-reviewed, proposed *task-directed retrieval policy*, not a human
maintainer consensus or executable proof of downstream repair. It is stricter
than the existing keyword contract, whose Rust tests intentionally associate
`bug` with recent-changes. Thus the measured unwanted scans are disagreements
with that proposed usefulness policy, not newly discovered Rust contract failures.
Two agents do not remove shared model/annotation bias.

The external repositories are entirely test-only. The earlier synthetic Pixel
corpora are development/mechanism material, not new held-out claims; no new
training occurred. Linked/correlated issues were grouped before predictions:
37 total families, 35 among the 42 scored rows. Examples include ripgrep ignore
semantics and Excalidraw Wordwall/localization families. A post-run audit found
no normalized exact overlap with the 100 earlier synthetic inputs; maximum
nearest-development character similarity was 0.426 and maximum cross-repository
similarity 0.171. This lexical audit was completed after the fixed run and changed
no row; it is not a pre-test certificate of semantic independence.

**Coverage is insufficient by construction of the observed sample:** by-concept
support 42; each of the four specialized labels support **0**; mixed families
**0**. The protocol required at least five positives per specialized label and
five mixed families. No post-result positive examples were added to rescue it.

## Frozen systems and measured quality

Reference: verified current-source binary from the previous replay, SHA256
`c866661ca42c19ac2424d2a8e93d90a6f494283c2c3784bd1768c2bbf482e8a7`.
A fresh fetch confirmed main `f7ae9d0`, with no Rust/Cargo difference from the
experiment HEAD. No new build was necessary. PID/socket ownership was checked
before evaluation with `lsof`; all 45 raw responses identify source HEAD and
clean runtime state. Only the owned experiment daemon was stopped afterward.

Candidates: the frozen conservative rules; the previous TF-IDF/logistic artifact
unchanged (four binary scores, threshold 0.5, exclusive concept fallback); and
always-by-concept. The rules were frozen before the parent read test annotations.
The model was not fitted again. Candidate, corpus, gold reports, protocol and
evaluator hashes were all recorded before test inference. No post-test variant.

| 42 scored requests | Current-source rules | Conservative rules | Frozen linear | Always concept |
|---|---:|---:|---:|---:|
| Exact-set correct | 25/42 | 42/42 | 42/42 | 42/42 |
| Exact-set accuracy | 59.52% | 100% | 100% | 100% |
| Five-label macro-F1 | 0.149254 | 0.200000 | 0.200000 | 0.200000 |
| Extra specialized labels | 20 | 0 | 0 | 0 |
| Missing concept labels | 17 | 0 | 0 | 0 |
| Cost/task, FP + 2 FN | 1.285714 | 0 | 0 | 0 |

Macro-F1 uses the preregistered five labels and zero for unsupported classes;
with four labels absent from gold, even perfect concept prediction yields 0.2.
Zero specialized omissions is vacuous here: specialized recall is unmeasured.
The three candidates tie the trivial control, so neither model complexity nor
the proposed mixed-query policy earns adoption from these scores.

Per repository, existing rules score 8/15 ripgrep, 11/15 Ruff and 6/12 Excalidraw;
all controls score 15/15, 15/15 and 12/12 respectively. The descriptive family
bootstrap (2,000 samples, seed 42) gives the same delta interval for all three
controls: +23.68..+55.56 percentage points versus existing rules. It describes
this sample and proposed gold, not a production prevalence estimate or evidence
of positive-intent safety across repositories.

Concrete existing-rule disagreements include:

- [ripgrep #3526](https://github.com/BurntSushi/ripgrep/issues/3526): hyperlink path
  encoding selects dead-interactive and recent-changes, dropping concept lookup.
- [Ruff #28721](https://github.com/astral-sh/ruff/issues/28721): the diagnostic name
  `unused-function-argument` selects dead-code despite a targeted Unicode bug.
- [Excalidraw #12145](https://github.com/excalidraw/excalidraw/issues/12145): adding
  context-menu icons selects dead-interactive rather than locating menu code.

Raw results retain all 17 error IDs and per-label counts: dead-interactive FP 5,
dead-code FP 4, hotspots FP 0, recent-changes FP 11. No further patterns were
added to the frozen rule implementation after inspecting these errors.

## Resources and checks

Quality run: **11.35 seconds**, process peak RSS **192,659,456 bytes** (~184 MiB),
including Python/sklearn and stored raw responses, excluding the daemon. No
neural download, training, global configuration change or compilation this turn.
The host was **85.82% idle**, only ~144 MB unused RAM: **performance not validated**.
No dedicated latency repetition was launched and no user process was stopped.

Incidental observations: source-daemon readiness 0.495 s; serialized model/runtime
load 0.754 s; first rule request 0.667 ms, remaining 44 median/p95 0.158/0.639 ms;
first linear request 6.786 ms, remaining 44 median/p95 1.364/2.945 ms. Actual Plan
median/p95 131.746/270.694 ms includes retrieval and is not a routing-only timing.
The model load excludes earlier imports/mechanism work. Cache state is uncontrolled.
These clocks cannot establish a deployment speedup or compare with previous runs.

Executed checks: **16 mixed-job contract tests + 5 corpus/gold tests + 8 existing
pilot tests**, Python compilation, actual three-job source-daemon mechanism check,
45/45 Rust/Python baseline name-set parity, literal evidence validation for 45/45
annotations, and independent direct exact/FP/FN recomputation. `scripts/ci/gates.sh`
passed **67 tests (36+12+9+10)** and explicitly skipped Cargo gates for docs/bench
changes. No Rust edits, install/doctor, full workspace tests, mutants, push or PR.
Three initial contract-test failures (unterminated fences, concept negation suffix,
relational scope) were fixed before candidate freeze/gold reveal; their log is
retained. The actual quality run had no failures or omitted prediction rows.

The independent follow-up audit reproduced all 45/42-row scores, verified every
freeze digest and literal quote, and confirmed that the three candidate outputs
are row-for-row identical. It identified two mechanism-evidence gaps. A separate
post-review supplement closes them without rerunning quality or changing code:
`supplement/evidence.json` records the unsupported input/hash, rejection and zero
transport-call ledger, then preserves a real source-daemon missing-prompt error
through the executor's exception. That server error is an explicit wire-level
fault injection into an otherwise valid job, not a naturally occurring graph
failure. The second owned daemon also exited cleanly. Initial audit and supplement
are both retained; the original mechanism record was not rewritten.

## Artifacts and reproduction

- `scripts/bench/bench-plan-mixed.py`: standalone routing and opt-in explicit-job execution.
- `scripts/bench/bench-plan-real-corpus.py`: deterministic selection from API captures.
- `scripts/bench/bench-plan-real-evaluate.py`: frozen comparison and raw evidence.
- `scripts/fixtures/plan-routing-real-{requests.json,gold.jsonl}`: versionable corpus.
- `.pixel/experiments/plan-routing-real-v1/`: API snapshots/exclusions, source pins,
  both annotation reports, contract audit, freeze manifests, exact command,
  `eval-01/{raw.jsonl,summary.json,mechanism.json}`, ownership and teardown receipts,
  host/resource logs, initial failures and successful gates.
- Durable archive: `~/code/pixel-experiment-artifacts/plan-routing-real-v1.tar.gz`.
  It also retains the verified binary/model under `inputs/` and scripts/fixtures
  under `reproduction/`. Earlier archives retain their original build/training
  provenance; the replay itself does not retrain.

Key SHA256 identities:

| Artifact | SHA256 |
|---|---|
| Deterministic candidate | `8121e04bc9cf3c1d2d955bc52a9d62bf3e2d585c0d19d789701f5bd3537a0d6b` |
| Evaluator | `c571fcb2e202e8f7b4fc3151b31039c24d7b5521839a3cc88ecac35515c74560` |
| Real requests | `9ce9593b73949af02c52535b00a5471d5206cc795c10363562931a99f5e3d42e` |
| Reviewed gold | `a623ceff0194937c846ea3a67821ab70901dc93df659c9cddcd23f911172b807` |
| Frozen linear artifact | `da088ca3b289af9370c0898a23e8aad52d06997f034c64a83a206cc078ebf37c` |

With the retained venv/model and an isolated current-source daemon, output paths
must be fresh:

```sh
python3 scripts/bench/bench-plan-mixed.py 'Locate classify_prompt; rank highest fan-in files'
# Add --socket only to execute the independent read-only jobs.
.pixel/experiments/plan-routing-pilot-v1/venv/bin/python scripts/bench/bench-plan-real-evaluate.py \
  --requests scripts/fixtures/plan-routing-real-requests.json \
  --gold scripts/fixtures/plan-routing-real-gold.jsonl \
  --candidate-freeze .pixel/experiments/plan-routing-real-v1/candidate-freeze.json \
  --model .pixel/experiments/plan-routing-pilot-v1/fit/model.joblib \
  --socket /socket/of/isolated/current-source/daemon --output /new/results
python3 scripts/test-bench-plan-mixed.py
python3 scripts/test-bench-plan-real.py
scripts/ci/gates.sh
```

## Decision

Retain the prototype, corpus and evidence as experimental. The real requests
confirm a policy problem with isolated keyword triggers in issue descriptions,
but adding a learned model buys nothing over a constant on this sample. Do not
integrate a default change or claim specialized/mixed intent quality is solved.

The useful next data source is **actual requests to Pixel's planning surface**,
including affirmative audits and mixed operations, rather than more generic bug
reports. Freeze those families and adjudicate whether their requested composition
is an independent union or a scoped intersection. Only then can a candidate's
specialized recall and cost be measured. This is a concrete remaining evidence
gap, not a reason to repeat general-model benchmarking or buy larger weights.
