# Critical review of #1241 — 2026-10-19

Verdict: **improvement proved on evidence qualification**, with a residual coverage
regression explicitly accepted as removal of a false association. This report
supersedes the initial candidate conclusion in the parent directory, whose
historical inputs and metrics are preserved. #1197 stays closed; #1231 is the
follow-up addressed by the PR.

## Findings and fixes

1. Removing the entire second lexical tier lost ripgrep-07's `run` selector and
   conditions. Hypothesis: restore bounded secondary snippets as independent
   unresolved references, without composing them into a path. The scratch
   prototype recovers those conditions; the native comparison below recovers
   the regression. Every call header says receiver/binding unresolved and scope
   unverified; secondary headers add `independent context, no composed path`.
   Direct matches are visited first, and deduplication includes the queried name,
   so an indirect homonym cannot suppress a direct site in the same function.
   Ambiguous origin names are not expanded into a secondary tier.
2. Python's preceding-function regex assigned global constants to an unrelated
   function. Actual AST function spans now determine lexical membership; invalid
   Python abstains from identifying a function. For other languages the header
   says `near … (scope unresolved)` rather than claiming containment.
3. `ast.walk(handler)` attributed nested function/async/class raises to the outer
   handler. Traversal now prunes those lexical scopes. A class body can execute
   during exception handling; its raises still belong to the class scope and
   are not labelled as the outer handler's local conversion.
4. An import name can be shadowed. Its header now says `dependency boundary
   candidate: matching import name; binding unresolved; external emission not
   inspected`. This is evidence of import syntax, not a resolved dependency edge.

The rejected #1197 name-only caller prototype remains rejected. The alternative
of recovering coverage by reattaching globals to the previous definition is also
rejected: it fails the global-scope negative control. No repository paths, error
strings or benchmark answer names are built into the probe.

## Controls and failure without the correction

`python3 -I /tmp/pixel-1241-review/reproduce.py /tmp/pixel-1241-review/candidate`
passes 4/4 scratch controls (global scope, nested raise, context conditions,
shadowed import), versus 0/4 on PR head `1114d9b995e0a72cccfebde47b9f221f601cb4ac`.
`python3 -I /tmp/pixel-1241-review/red.py` loads the committed test module but
substitutes the old executable from `git show 1114d9b9:probes/error-origin/probe`:
5 review regression methods, 7 failing assertions, 0 errors, exit 1. This includes
three nested-scope subcases and the direct/indirect homonym collision. To repeat
without the session scratch script, load `test_probe.py` with importlib, replace
its module-level `PROBE` with that executable, and run `ReviewRegressions`.

After correction, `python3 -I probes/error-origin/tests/test_probe.py` passes
16 tests, including the original doc-only, receiver-homonym, genuine emission,
message-definition, conversion and rethrow controls. `python3 -I
scripts/bench/probe-test.py --self-test` passes 24 tests. Only output-contract
assertions change in that harness; the common scorer is unchanged.

`python3 -I /tmp/pixel-1241-review/offline.py` uses the shipped judge on fixed
native baseline briefs: **OK**, 56 questions, 7 executions, deterministic x2,
read-only, no timeout/error/stray output. This is a diagnostic replay, not an
agent answer-quality assessment. The original lexical guard flags remain
manually reviewed as generic AST vocabulary and epistemic prose.

## Frozen three-arm comparison

The corrected common protocol remains **v4**, from #1230/#1235 via #1240;
scorer SHA-256 `95bf349407a86091b7627f0d7726697478408b743345b32d4ed919da36233106`.
Same native source `a0d99d6268b770b49bf44effef415ac95b1252aa`, binary SHA-256
`fcde2a41d4d0e7017acdba2ee58e4803afa9379b8b3995b68e24b71ec14d8e64`,
questions SHA-256 `984a632e96f5f32891279d58f872c3a891bfeabaee110b3993be313c63e17069`,
seven repository pins and all other probes. Warm hook/Pi brief_only, three parts /
3000 ms, process timeout 300 s, dedupe off. Measurements and offline judge ran
sequentially under `/tmp/pixel-benchmark.lock`. The baseline's 56 brief texts
reproduce the previous run exactly. Both new arms complete with exit 0:
53 ok + 3 abstained, no errors, cuts or partials; abstentions stay in the mean.

The reviewed probe was frozen in unpublished local fixture commit
`7a494c52db3d27bea41df6f55b5d63b961eb4d9b` (not a publication commit), so that the
native protocol could consume an immutable commit before the final signed code
and evidence commit. Program SHA-256
`5ed40604621a1d3f6ece8b7eeae3669a91521d946fc0522d013535a78feb5ffc` and manifest
SHA-256 `56f8b3a0f3258d1b647e47e6763bb663e216200e45433e0d4cf98f713f38be06`
identify the measured bytes. Final publication must preserve those bytes. Exact
argv, 28 probe hashes, seven pins, reports and control hashes are in
[identity.json](identity.json). For replay from the published PR head, substitute
`--head-probe-commit HEAD --repo "$PWD"` and verify those two hashes first:

```sh
python3 -c 'import fcntl,subprocess,sys; f=open("/tmp/pixel-benchmark.lock","a"); fcntl.flock(f,fcntl.LOCK_EX); sys.exit(subprocess.call(sys.argv[1:]))' \
  python3 -I scripts/bench/tool-census/score_brief.py pinned \
  --base /tmp/pixel-1197-audit/pixel \
  --probe-home /tmp/pixel-1197-audit/runs/probe-home-a0d99d6268b7 \
  --head-probe-commit HEAD --repo "$PWD" \
  --questions "$PWD/eval/brief-bench/questions.jsonl" --route hook \
  --root /tmp/pixel-1241-review/state --out /tmp/pixel-1241-review/replay \
  --label review1241-replay
```

Baseline = frozen a0d probes; before review = measured b634bd8c probe bytes,
unchanged in published PR head 1114d9b9; reviewed = local immutable fixture above.

| Category | n | file: baseline / before review / reviewed | region: baseline / before review / reviewed |
|---|---:|---|---|
| all | 56 | 0.425378 / 0.425378 / 0.424004 | 0.084869 / 0.087198 / 0.086742 |
| config | 7 | 0.370308 / 0.370308 / 0.370308 | 0.085233 / 0.085233 / 0.085233 |
| diagnosis | 1 | 0.263158 / 0.263158 / 0.263158 | 0.120877 / 0.120877 / 0.120877 |
| error-origin | 7 | 0.254737 / 0.254737 / 0.243748 | 0.050879 / 0.069507 / 0.065860 |
| flow | 7 | 0.366213 / 0.366213 / 0.366213 | 0.147829 / 0.147829 / 0.147829 |
| history | 7 | 0.791444 / 0.791444 / 0.791444 | 0.119940 / 0.119940 / 0.119940 |
| impact | 6 | 0.183952 / 0.183952 / 0.183952 | 0.000671 / 0.000671 / 0.000671 |
| lookup | 7 | 0.297357 / 0.297357 / 0.297357 | 0.063135 / 0.063135 / 0.063135 |
| overview | 7 | 0.796465 / 0.796465 / 0.796465 | 0.155627 / 0.155627 / 0.155627 |
| tests | 7 | 0.331233 / 0.331233 / 0.331233 | 0.038466 / 0.038466 / 0.038466 |

All 56 rows and both deltas are in [per-question.jsonl](per-question.jsonl);
category values are in [per-category.jsonl](per-category.jsonl). 53/56 questions
keep coverage metrics versus baseline, 50/56 briefs remain byte-identical.
Only the following three questions change metrics:

| Question | file: baseline / before review / reviewed | region: baseline / before review / reviewed |
|---|---|---|
| ripgrep-07 | 0.105263 / 0.105263 / 0.105263 | 0.003872 / 0.003125 / 0.003895 |
| requests-07 | 0.750000 / 0.750000 / 0.750000 | 0.005658 / 0.136806 / 0.136806 |
| flask-07 | 0.461538 / 0.461538 / 0.384615 | 0.066415 / 0.066415 / 0.040112 |

- ripgrep-07: region coverage exceeds the original baseline slightly; line
  coverage 0.100588 → 0.081176 → 0.101176. The `run` selector and parallel
  conditions return as explicitly independent unresolved snippets. This recovers
  context, without proving that the lexical call names bind to the target.
- requests-07 retains the conversion/handler coverage gain. The import is only
  a candidate boundary, and urllib3's message construction is not inspected.
- flask-07: the original header wrongly said the module-level `_no_app_msg`
  definition at `globals.py:34` was `in _get_current_object`. Its subsequent
  caller/test snippets were derived from that false association. Removing them
  loses one oracle file and region coverage 0.066415 → 0.040112. This is a real
  retrieval regression; no answer-quality improvement is inferred from it. A
  future generic use/alias resolver could recover justified context separately.

## Limits and propagation

Producer: probe stdout. Consumers: unchanged bundled registry/native hook merge,
probe-test and common v4 scorer, retaining the numbered-line/header contract.
Manifest 0.3.0 remains this PR's version. No stored graph field, extractor,
wire protocol, scorer or install target changes. Source scans and output caps
remain bounded. Two-tier lexical context can still contain irrelevant homonyms;
the output explicitly qualifies them and cannot establish a composed runtime
path. Function membership is lexical only. Handler selection is bounded and
heuristic; arbitrary bindings, external emitters and actual call paths remain
unresolved. No real-agent campaign, quality-reviewed zero-call claim, local
installation, Rust change or mutation campaign. CI must validate the final
publication SHA before merge.
