# Public acceptance set for the prompt brief (#906)

Maintainer decision, 2026-10-17: these **56 public questions on seven pinned
repositories** replace the unavailable private 13-question census as the
acceptance set for #906. The private experiments remain historical evidence.
Question wording and repository pins are unchanged by this repair.

## Evidence and provenance

`questions.jsonl` contains source ranges in `oracle`, their summed size in
`oracle_lines`, and optional full commit IDs in `oracle_commits`. History
commit IDs are scored independently from source-line coverage. The scorer
carries them through the question source into `commit_recall`; quoting a
release note alone does not count as naming its commit.

`oracle_source` identifies a manual source review or a recorded run. The
historical `baselines.jsonl` remains unchanged: repairing expected evidence
does not fabricate new baseline tool calls. Fresh paired runs supply the
acceptance control. Coverage is diagnostic, not a semantic-quality grade.

The five empty oracles were repaired by inspecting the pinned Git objects:

| Question | Required evidence | Provenance |
|---|---|---|
| requests-03 | `trust_env` / `merge_environment_settings`, `get_environ_proxies`, bypass and proxy resolution, documented lower/upper case variables | `sessions.py`, `utils.py`, `docs/user/advanced.rst` |
| flask-04 | Context push, full dispatch, preprocessing, view dispatch, response finalization, both error paths, response WSGI call and context pop | `src/flask/app.py` |
| flask-05 | Python >=3.11, removed 3.10 support, compatibility and workflow changes | `CHANGES.rst`, `pyproject.toml`, installation docs; commit `7689f98403e2c0b0bc8d1031e60f8cff2620e86a` |
| express-04 | Application callable, initialized lazy router, request/response setup, middleware mounting and router/finalhandler boundary | `lib/express.js`, `lib/application.js`; external router internals are outside this repository |
| express-05 | 5.3.0 release changes and version bump | `History.md`, `package.json`; release commit `cf6722bf89a86905281d9160b6482e08166b5ef0` |

Six Pixel questions also named 408 paths absent at their unchanged `fe0a1eab`
pin (41, 61, 110, 27, 97 and 72 ranges in pixel-02/03/04/06/07/08). Those
ranges were removed, not guessed at replacement locations. The existing
scorer already ignored absent files; the correction makes the serialized
oracle match that denominator. Remaining ranges were checked at the pin.

After the shared cache has been prepared by the scorer or live runner:

```sh
python3 -I scripts/bench/brief-bench.py check --verify-oracles --root /path/to/state
```

This is an offline check against `git show <pin>:<path>`: every range must be
inside a tracked file at the pin, and every history commit must be an ancestor
of the pin. Missing caches fail explicitly; the check never fetches or edits
repositories. Ordinary CI `check` rejects empty oracles, invalid commit IDs and
inconsistent line counts. Baseline generation refuses to overwrite the bench
with an empty oracle from an answer produced only from memory.

## Acceptance protocol (defined before the new live campaign)

1. Freeze the native binary and its bundled probe programs at one source SHA,
   using an isolated Pixel home. Record their hashes, the question-file hash,
   agent version/model/effort, hook parts, timeouts and concurrency. Do not mix
   the 2026-10-17 **51-oracle** baseline with the repaired dataset.
2. Run all 56 questions in fresh pinned workspaces with and without the brief.
   Same model, effort and question; three hook parts in the brief arm and no
   Pixel hooks in the control. Build history for history questions. Report
   protocol failures and missing delivery separately from numeric call counts.
3. Review the final answers against the question and its pinned repository,
   including Git history when requested. The oracle guides inspection; it is
   not the entire truth and cannot excuse an unsupported answer. Record each
   answer hash, source SHA, findings and quality verdict outside raw streams.
4. An answer must address the requested behavior, configuration, path, flow,
   error handling or change accurately. Material omissions, wrong versions,
   unsupported assertions or citations that contradict the claim fail quality.
   Different wording is fine. The brief answer must be at least as correct and
   complete as the control, and both must meet the question's requirements;
   equally wrong answers never pass. Inconclusive cases remain unaccepted.
5. A candidate zero-call success needs completed, nonempty, quality-passing
   answers and all expected brief parts delivered. Repeat each such pair twice
   more: count it as accepted only after three brief runs remain at zero with
   quality and delivery intact. Report every repetition and min/max counts.
   One pair can diagnose a failure, but cannot establish a reliable reduction.
6. Publish per-question delivery, call counts and reviewed quality separately.
   Any failed question keeps the full-set acceptance goal open. Do not tune a
   probe on this set and present the same set as held-out validation; #906 is
   a development acceptance set, not a generalization benchmark.

Raw answers, commands and streams stay in the local run directory. Committed
reports contain aggregates, answer hashes, source references and review
findings, never a copied private transcript. A zero-call runner exit code
checks delivery and calls only; the reviewed campaign is the acceptance result.

## Reproduce the acceptance verdict

`brief-zero-calls.py` can verify the aggregates without another model run:

```sh
python3 -I scripts/bench/brief-zero-calls.py \
  --acceptance-run /path/to/full-first-run \
  --acceptance-run /path/to/candidate-repeat-2 \
  --acceptance-run /path/to/candidate-repeat-3 \
  --quality-reviews /path/to/reviews.jsonl > acceptance.json
```

Each directory supplies `report.json` from the live runner and an `identity.json`
from the freeze/campaign launcher. The manifest contains `run` (directory name),
`questions_sha256`, `native_sha256`, `wrapper_sha256`, `probe_files` (relative
program/manifest names to hashes), `harness_source_sha`, `agent`, `model`,
`effort`, `parts`, `jobs`, `index_jobs`, `timeout_s`, and `history`. Repeats
also declare `selected_ids`; the first run must cover the complete dataset.
These manifests record actual frozen inputs; supplying matching labels is not
a substitute for collecting their hashes before running the experiment.

Each review JSONL row identifies `run`, `id`, full `source_sha`, and `brief` /
`nobrief` objects with `quality` (`pass`, `fail`, `inconclusive`) and the
`answer_sha256` copied from that run's aggregate report. Hashes identify the
UTF-8 final answer after stripping surrounding whitespace. Include `reason`
and pinned `sources` so a maintainer can assess the source review.

The verifier rejects stale hashes, wrong pins, changed experiments, duplicate
or incomplete pairs and an incomplete first run. Missing reviews, errors,
empty answers, missing/late parts, nonzero brief calls or fewer than three
pairs prevent acceptance. It exits 0 only when **every question in the selected dataset** passes (56 by default); it does not
judge answer meaning. Quality verdicts still require the source review above.
The output's call totals use only the full first pass, so selected repetitions
do not bias the aggregate comparison.

## Latency lane

`.github/workflows/perf.yml` scores these 56 questions every Monday with
`scripts/bench/tool-census/score_brief.py`, through the prompt-submit hook
route (3 s window, probe floor) and with the commit's own probes frozen by
`freeze-home`. `scripts/ci/check-perf.py --label brief` compares the report
with `.github/perf-baseline.json`: p95 and max `elapsed_ms` at three times
the baseline, at most three `partial` briefs (cut, or past `deadline_ms`),
and all 56 timed with the baseline's question-file hash. Baseline, GitHub
`ubuntu-26.04` runner, 2026-10-19: p95 885 ms, max 1,042 ms, 0 partial.
A change to `questions.jsonl` refreshes the baseline in the same pull request.

## Published campaign

[2026-10-17 evaluation](../../docs/bench/brief-public-acceptance-2026-10-17/README.md):
9/56 accepted after three paired repetitions; 78 brief versus 235 fresh control
calls in the full first pass. The report carries the frozen inputs, per-question
source reviews and aggregates to reproduce the failed full-set verdict offline.

[2026-10-18 evaluation](../../docs/bench/brief-public-acceptance-2026-10-18/README.md)
(`main` `904d9570`, after #1208–#1215): 18/56 accepted; 64 brief versus 231
fresh control calls in the full first pass. Reviews by Claude sub-agents.
