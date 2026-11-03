# Common scorer reference — 2026-10-18

**Protocol v3, after #1230 / #1235. Measurement correction, not retrieval improvement.**
Stack: [#1236](https://github.com/Pixel-CLI/pixel/pull/1236) → [#1237](https://github.com/Pixel-CLI/pixel/pull/1237). Neither PR is merged.

## Identity

- Scorer code commit: `1261ba4665c6e637f5ce7ac3b6d50592eeffbe5c` (upper PR; later evidence-only commits do not change it).
- Scorer SHA-256: `febb6031075e4f20f3e2380451ce7b88cbb7613b89ae904252987dc756d3889e`.
- Binary source `a0d99d62`, dev-release, SHA-256 `fcde2a41d4d0e7017acdba2ee58e4803afa9379b8b3995b68e24b71ec14d8e64`.
- Probes frozen at `a0d99d62`: 14 programs + 14 manifests; every hash is in [identity.json](identity.json).
- Questions SHA-256 `984a632e96f5f32891279d58f872c3a891bfeabaee110b3993be313c63e17069`; same 56 questions and seven verified repository pins.
- Hook Pi brief_only, warm, 3 parts / 3000 ms, dedupe off, process timeout 300 s, sequential arms under `/tmp/pixel-benchmark.lock`.
- Inputs and source-report hashes are in identity.json; [per-question.jsonl](per-question.jsonl) includes every frozen brief hash. Historical reports have not been rewritten.

## Same frozen texts, old/new measurement

v1 → v2 changes validity/exit behavior: **0/56 questions change coverage** on these healthy inputs.
v2 → v3 changes attribution: **31/56 questions change at least one metric**, without changing a single brief byte.

| Metric | v1 = v2 | v3 |
|---|---:|---:|
| file_recall | 0.425378 | 0.425378 |
| region_cov | 0.092839 | 0.084869 |
| line_cov | 0.126137 | 0.110799 |
| inline_cov | 0.076003 | 0.093815 |

False code coordinates and out-of-range rows no longer earn line/region credit; later docs/code blocks regain their own quoted coordinates. File recall remains identical. An inline increase is a corrected attribution, not new evidence or fewer agent calls.

| Category | n | region v2 → v3 | line v2 → v3 | inline v2 → v3 |
|---|---:|---:|---:|---:|
| config | 7 | 0.090532 → 0.085233 | 0.102826 → 0.100974 | 0.042436 → 0.097226 |
| diagnosis | 1 | 0.120877 → 0.120877 | 0.205937 → 0.205937 | 0.003711 → 0.205937 |
| error-origin | 7 | 0.052231 → 0.050879 | 0.114876 → 0.112080 | 0.063776 → 0.111996 |
| flow | 7 | 0.161038 → 0.147829 | 0.175645 → 0.163348 | 0.105431 → 0.119420 |
| history | 7 | 0.119940 → 0.119940 | 0.119929 → 0.119929 | 0.047619 → 0.047619 |
| impact | 6 | 0.004374 → 0.000671 | 0.018504 → 0.009878 | 0.016328 → 0.009878 |
| lookup | 7 | 0.078894 → 0.063135 | 0.175046 → 0.151247 | 0.146437 → 0.150005 |
| overview | 7 | 0.160570 → 0.155627 | 0.147570 → 0.105579 | 0.087464 → 0.092797 |
| tests | 7 | 0.058489 → 0.038466 | 0.127923 → 0.095349 | 0.100338 → 0.093570 |

Exact file/region/line/inline values and deltas for every question: [per-question.jsonl](per-question.jsonl); exact category values: [per-category.jsonl](per-category.jsonl).

## Native #1201 pipeline

Two full runs with the same binary and frozen home: **56/56 byte-identical briefs**, also identical to the old frozen texts; **0 metric cells differ**, all 56 rows match the v3 replay. Both commands exit 0 and report complete: 53 ok + 3 abstained; no errors, cuts or partial results. Abstentions (`requests-06`, `fd-06`, `fd-08`) stay in the denominator.
p95 internal latency: 769 / 703 ms (descriptive; equality is not required). No performance gain is claimed.
All session metrics and statuses from both native runs: [native-repeat.jsonl](native-repeat.jsonl).

## Commands to reuse

Set `SCORER` to the v3 scorer above, `BINARY` to the verified frozen binary, `PROBE_HOME` to its verified frozen home, and `QUESTIONS` to the pinned JSONL. `ROOT` is a dedicated state directory and `RUN` a fresh label. Verify their SHA-256 values against identity.json before measuring.

```sh
python3 -c 'import fcntl,subprocess,sys; lock=open("/tmp/pixel-benchmark.lock","w"); fcntl.flock(lock,fcntl.LOCK_EX); sys.exit(subprocess.call(sys.argv[1:]))' \
  python3 -I "$SCORER" pinned --base "$BINARY" --head "$BINARY" \
  --probe-home "$PROBE_HOME" --questions "$QUESTIONS" --route hook \
  --root "$ROOT" --label "$RUN"
```

For one content/probe candidate, keep the binary fixed: replace `--head "$BINARY"` with `--head-probe-commit "$CANDIDATE_REV" --repo "$CONTENT_REPO"`. The base remains the frozen home above. For a binary-only comparison, replace only `--head`. Never change both binary and probes in one arm.

To reproduce the fixed-text scoring comparison, use Git worktrees at the revisions in identity.json for v2/v3, so each scorer records its own checkout's commit. Legacy v1 can also be exported with its sibling classify.py/question_source.py. Keep the explicit frozen question file:

```sh
python3 -I scripts/bench/tool-census/replay_briefs.py \
  --before "$V2_SCORER" --after "$SCORER" \
  --report "$FROZEN_REPORT" --briefs "$FROZEN_BRIEFS" \
  --binary "$BINARY" --probe-home "$PROBE_HOME" --questions "$QUESTIONS" \
  --root "$ROOT" --out "$FRESH_REPLAY_OUTPUT"
```

The replay tool takes the same common lock, checks binary/questions/probe hashes and pins, refuses overwrites and verifies that brief bytes did not change. It exercises each scorer CLI with frozen hook results while substituting only index activation/execution/cleanup; it does not measure retrieval or latency. The separate unmocked native pair verifies that seam end to end. Exact expanded argv and complete raw reports are retained in the local audit evidence bundle (`commands.json`, SHA recorded in its manifest).

## Regression evidence and limits

- #1230: three new tests on the unmodified v1 scorer fail (`0 != 1` for process/pinned, no cleanup). v2 passes 35 self-tests.
- #1235: the three parser tests on v2 produce seven failing assertions. v3 passes 38 self-tests, including the false code-oracle control, adjacent code/docs and docs/docs, same-file/empty/unknown headers, clipped numbered rows and fenced cases.
- Actual optional-probe failure/empty/timeout safety from the #1201 audit remains valid because the native binary is unchanged; v2/v3 also test that an optional timeout does not fail a valid brief.
- No real agent run or answer-quality review here. No zero-call claim. Models/cache contents, PATH tool versions and OS are not frozen by #1201; no cross-machine reproducibility claim.
- PR CI, not historical runs, validates each published head. No merge requested.
