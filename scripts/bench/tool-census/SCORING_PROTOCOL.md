# Brief oracle scoring protocol

This versions the offline measurement, independently of the binary, probes and
live answer-quality acceptance. Historical reports are immutable.

## Version 1 (legacy, through #1201)

Reports without `protocol` used the original parser and diagnostic aggregation.
An exit 0 did not qualify process/output validity. Their coverage is diagnostic;
do not retrofit a successful acceptance status onto them.

## Version 2 (#1230)

New reports name `protocol`, the scorer's Git commit and file SHA-256, binary,
probe manifests/programs, question hash, repository pins, selection, route,
warm/cold state and per-process timeout. `pinned` holds the probes/questions
constant; serialize benchmarks with `/tmp/pixel-benchmark.lock`.

| Row status | Credited coverage | Final CLI status |
|---|---|---|
| `ok` | observed context | complete / 0 |
| `abstained` | zero, **kept in the denominator** | complete / 0 |
| `process_error` | zero; raw context/stderr retained | failed / 1 |
| `malformed` | zero; response/error retained | failed / 1 |
| `explicit_error` | zero; raw context/error retained | failed / 1 |
| `partial` | observed context, diagnostic only | failed / 1 |
| `timeout` | zero; partial process output retained | failed / 1 |

Precedence: timeout, process exit, malformed response, explicit error, partial,
then nonempty context or abstention. An envelope must be an object and its
context/brief a string; absence is allowed for a closed gate or explicit error.
The legacy plain-text command route has no JSON schema to validate.

An optional probe's `failed`/`empty`/`timeout` outcome **does not** invalidate a
native brief. Those outcomes are recorded as evidence, not confused with the
brief process timeout. `--timeout` defaults to 300 seconds; it does not change
the native hook's 3000 ms window. A native cut or elapsed time over its window
marks a partial result. Zero calls and semantic quality require the separate
paired live protocol in `eval/brief-bench/README.md`.

All selected rows, including zero-coverage abstentions and errors, enter the
aggregate. Preparation/fatal/cleanup errors make the whole report incomplete;
its partial aggregate is not a full-set score. Reports are finalized after the
cleanup `finally`, including on exceptions. Failed activation also cleans its
unregistered clone. Cleanup attempts every clone and records failures.
`--keep-daemons` applies only to a completed run. `pinned` writes both arms and
the comparison, then propagates failure; `compare` also propagates a recorded
failure. Cross-version comparisons warn of a measurement change and cannot
establish a retrieval gain.

## Version 3 (#1235)

All v2 validity/denominator rules remain. Consecutive excerpt headers are
boundaries, including untracked/unknown source headers and a header after an
unterminated fence. Each numbered inline row must lie inside its own declared
range and the tracked file. A rejected excerpt's fence is consumed once; it
cannot be reinterpreted as a headerless excerpt outside that range.

Pointer coverage (`all`) and quoted coordinates (`inline`) remain distinct.
An empty excerpt may name a pointer without quoting its lines. A docs row may
not become a code coordinate belonging to the preceding block. Scores can
increase or decrease on byte-identical briefs because attribution is corrected;
these are measurement corrections, never retrieval gains. Reports from earlier
versions remain unchanged. Replay frozen briefs before running the complete
#1201 pipeline; publish both per-question and per-category comparisons.

## Version 4 (#1239)

All v2 validity and v3 attribution rules remain, with two additional source
ownership guards. A syntactic source header owns its body even when the file
cannot be resolved: its fence is consumed without global matchback. Truly
headerless fences still use global matchback.

If any body row carries an explicit line number, only those numbered rows are
credited as inline coordinates, after the same file/range/EOF bounds. This
also applies to mixed bodies below v3's 60% numbered-row threshold. Their
unnumbered rows are ignored rather than sequentially renumbering the block.
Wholly unnumbered blocks retain the existing sequential/bounded matchback
rules. Header pointers remain distinct from inline quotes.

These guards can remove false credit or restore an explicitly numbered row
that v3 discarded. Compare the same frozen briefs and record each question
and category; any delta is a measurement correction, not retrieval improvement.
Keep the v1/v2/v3 reports and their dated reference unchanged.
