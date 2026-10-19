# #906 integrated offline reference — 2026-10-19

Verdict: **référence utilisable** for the next offline #1234 comparison. This is a stable measurement of the combined integration, not answer-quality acceptance. The numerical `flask-07` regression and the existing content defects below remain explicit; no live campaign or zero-call claim.

## Frozen integration and provenance

Fetched `origin/main` and froze **`dbff5b09d35a00a07572751d556b330a9da38acd`** in a clean dedicated worktree before building or measuring. GitHub was reread: #1241 (`35916d9a`), #1242 (`de1563de`), #1243 (`c347dcca`) and #1244 (`dbff5b09`) are all **merged**. Their individual overlays do not prove that the combined packet has no budget interaction. #1201, #1230, #1235 and #1239 remain completed; #906 remains open.

- Binary: dev-release, default fastembed/model2vec features, SHA-256 `20bc41f6c9f2767733589ee6852e47c8400301f4835a8ad2edf996dda3c00988`; reports clean source `dbff5b09`, aarch64-apple-darwin, rustc 1.99.0. No personal install/setup/self-update was performed.
- Scorer: unchanged v4 SHA-256 `95bf349407a86091b7627f0d7726697478408b743345b32d4ed919da36233106`. Questions: unchanged `984a632e96f5f32891279d58f872c3a891bfeabaee110b3993be313c63e17069`, all 56 and the seven verified clean pins.
- Isolated probe HOME from the **same dbff5b09 commit**: all 14 programs and 14 manifests verified before/after. Compared to the reference, only config-registry/probe, error-origin/probe and error-origin/pixel-plugin.toml change. The binary also changes, so this is **an integration result, not a causal ablation of one PR**.
- Hook Pi brief_only, warm, three parts / 3000 ms, dedupe off, process timeout 300 s. The shared `/tmp/pixel-benchmark.lock` was held throughout both sequential candidate arms, including preparation. Each campaign owns its state/cache/clones.
- The published [v4 historical reference](../scorer-protocol-v4-2026-10-19/README.md) is unchanged. Its 56 native metric rows agree with its per-question file and every retained baseline brief hash. Binary/probe baselines remain a0d99d62; no historical issue score is substituted.
- The first cached Cargo wrapper build exited 0 but announced **0669a76a**, not dbff5b09. It was rejected **before any measurement**. A fresh target with real Cargo and empty rustc wrappers produced the accepted binary. This is a provenance guard, not a claim about the cache’s fully diagnosed root cause. The rejected hash/version and accepted toolchain are retained in [identity.json](identity.json).

## Combined results and repeatability

Both 56-question candidate runs are **complete, exit 0**, each **53 ok + 3 abstained**, no process/schema/explicit error, partial result or native cut. Abstentions `requests-06`, `fd-06`, `fd-08` remain in the denominator. All **56 brief hashes, statuses and non-latency metric rows match between repetitions**. Latency equality is not required: descriptive internal p95 is **906 / 884 ms**; the historical v4 pair was 657 / 705 ms. No performance claim.

| Metric (macro mean) | Historical v4 | Integrated r1 = r2 | Delta |
|---|---:|---:|---:|
| file_recall | 0.425377968 | 0.424004341 | -0.001373626 |
| region_cov | 0.084869149 | 0.086780779 | +0.001911630 |
| line_cov | 0.110799104 | 0.114340248 | +0.003541144 |
| inline_cov | 0.093814809 | 0.097366457 | +0.003551648 |
| bytes | 11042.464285714 | 11008.392857143 | -34.071428571 |

**Four questions change scored coverage; nine change text; 47/56 texts remain identical to the baseline.** Stable repetition does not establish equal or improved answers.

| Question | line_cov before → integrated | region_cov before → integrated | file_recall before → integrated | bytes before → integrated |
|---|---:|---:|---:|---:|
| ripgrep-07 | 0.100588235 → 0.101176471 | 0.003872283 → 0.003894928 | 0.105263158 → 0.105263158 | 10465 → 14220 |
| requests-07 | 0.047008547 → 0.205128205 | 0.005658436 → 0.136805977 | 0.750000000 → 0.750000000 | 9197 → 12426 |
| flask-03 | 0.188259109 → 0.246963563 | 0.007816511 → 0.010000411 | 0.181818182 → 0.181818182 | 13751 → 16139 |
| flask-07 | 0.065817410 → 0.046709130 | 0.066414598 → 0.040111787 | 0.461538462 → 0.384615385 | 21152 → 12826 |

| Category | n | file_recall before → integrated | region_cov before → integrated | line_cov before → integrated |
|---|---:|---:|---:|---:|
| config | 7 | 0.370307602 → 0.370307602 | 0.085233228 → 0.085545214 | 0.100974142 → 0.109360492 |
| diagnosis | 1 | 0.263157895 → 0.263157895 | 0.120877061 → 0.120877061 | 0.205936920 → 0.205936920 |
| error-origin | 7 | 0.254736528 → 0.243747517 | 0.050878767 → 0.065859821 | 0.112080104 → 0.132022906 |
| flow | 7 | 0.366213152 → 0.366213152 | 0.147828808 → 0.147828808 | 0.163348046 → 0.163348046 |
| history | 7 | 0.791443850 → 0.791443850 | 0.119940476 → 0.119940476 | 0.119929453 → 0.119929453 |
| impact | 6 | 0.183952077 → 0.183952077 | 0.000670777 → 0.000670777 | 0.009878190 → 0.009878190 |
| lookup | 7 | 0.297357405 → 0.297357405 | 0.063135065 → 0.063135065 | 0.151246908 → 0.151246908 |
| overview | 7 | 0.796464646 → 0.796464646 | 0.155627300 → 0.155627300 | 0.105578837 → 0.105578837 |
| tests | 7 | 0.331233363 → 0.331233363 | 0.038466442 → 0.038466442 | 0.095348762 → 0.095348762 |

| Oracle type (pooled line visits) | denominator | covered before → integrated | inline before → integrated |
|---|---:|---:|---:|
| definition | 26395 | 2032 → 2091 | 1609 → 1669 |
| usage | 1999 | 113 → 112 | 65 → 64 |
| tests | 6365 | 478 → 478 | 464 → 464 |
| config | 979 | 225 → 225 | 225 → 225 |
| docs | 3488 | 371 → 371 | 364 → 364 |
| git | 2 | 2 → 2 | 0 → 0 |
| list | 0 | 0 → 0 | 0 → 0 |

Definition evidence gains 59 line visits; usage loses one. Tests, config and docs pools are unchanged, including docs **371/3488**. `git` counts items, not source lines; `list` has zero oracle items and is **not measurable**, not a successful zero. Config **question-category** improvement is definition evidence; it is not an increase in the config oracle type.

Full records: [per-question.jsonl](per-question.jsonl) contains every baseline/r1/r2 status, metric, byte count, cut/deadline/probe outcome and brief hash; [per-category.jsonl](per-category.jsonl) contains all nine category aggregates and type pools; [summary.json](summary.json) contains overall pools/statuses. Historical per-category hook latency metadata is unavailable in the published row file and is represented as null rather than invented.

## Source review, lost evidence and #1234 boundary

Every changed-text question was checked against its pinned source. [source-review.jsonl](source-review.jsonl) records exact paths/ranges, findings and uncertainty for all nine, plus the existing controls. [evidence-delta.jsonl](evidence-delta.jsonl) lists every question’s gained/lost inline ranges and those outside its oracle. These are source-coordinate diagnostics, not semantic answer scores.

- **requests-07** gains the actual MaxRetryError handler/conversions; the external urllib3 message emitter is still outside the inspected source. **flask-03** gains dynamic filename forwarding and conditional mapping writes; `silent` is not development/production selection and no effective precedence is asserted. **ripgrep-07** retains serial/parallel no-search conditions with explicit lexical uncertainty; extra independent name matches remain noise candidates.
- **flask-07** loses 9 oracle lines, one oracle file and 145 other inline lines when a global message stops being falsely assigned to a preceding TYPE_CHECKING protocol method. The removed snippets include real context-consumer examples, but they did not prove that call path. The remaining packet still omits `globals.py:40-48` LocalProxy construction; a more honest, smaller packet is not proved equally complete. This is the one numerical regression, retained in every table rather than averaged away.
- **flask-04:** oracle scores are identical, while **84 inline lines outside the oracle disappear**, mainly a false chain rooted in test-client documentation. The historically evicted **debughelpers.py:64-81 remains absent (0/18)** in the reference and both integrated repeats. `app.py:560-586` establishes its relevance to debug redirect/error handling. The same `docs/lifecycle.rst:1-9` and `docs/patterns/streaming.rst:64-100` blocks remain. We did not rerun the earlier docs-removal ablation or attribute the present absence causally to a single merged change. This is a persistent #1234 acceptance case.
- **Pure-code controls persist:** pixel-02 still carries historical experiment/review-gate prose instead of reliable current cap constants. fd-02 still carries root-directory examples and -x parallel-job documentation, which does not establish the default search-thread formula. requests-01 still has **0/240 docs/api.rst oracle coverage**. These unchanged gaps are not accepted as solved.
- **Docs packet audit:** all **54 doc-sections blocks in 31 questions** retain valid tracked paths/ranges and complete numbered rows in both arms. **26 source rows are shortened to 160 characters**; pixel-01 repeats **11 ARCHITECTURE.md source rows** across native/docs blocks. This exact cohort reproduces the earlier #1234 audit; these are not fixed by integration.
- **Conservative masking:** ripgrep-03 replaces four auxiliary config-read rows by `<masked>` while retaining their coordinates and the RIPGREP_CONFIG_PATH key. Coverage equality is not equal visible information. excalidraw-03 adds three honest declaration identities for a library service, but still does not answer the theme/persistence question (zero oracle coverage).
- **New bounded scorer/producer limitation, #1245:** express-01 prints a root README header with the wrong case; suffix resolution assigns its Readme.md text to examples/README.md. Correcting only that header in a scratch diagnostic changes **none of this question’s four current code-oracle metrics**. No other explicit candidate source header required suffix fallback. Do not treat the 29 false outside-oracle coordinates as docs evidence. [Diagnostic](express-path-diagnostic.json), [header audit](header-path-audit.json). Shared v4 was not changed and official reports were not rescored.

## JavaScript syntax facts actually integrated

All seven fresh graph databases report extractor **30**. The native artifact stores syntax facts for **171 JS files / 649 assignments**, including Express **141 / 595**, Excalidraw **26 / 47**, Pixel **4 / 7**; no fact truncation was recorded. [graph-facts.json](graph-facts.json) retains counts per pin. The #1242 table has no resolver/brief consumer; these are syntax facts, not new callable targets or receiver resolution. Their storage is verified even though the present oracle deltas do not demonstrate retrieval gains from them.

## Reproduce without a personal installation

Keep a source checkout pinned at the measured SHA; set `REPORT_DIR` to this report directory in the PR checkout. Set an isolated `SOURCE`, `BUILD_DIR`, `PROBE_HOME`, `STATE` and a **fresh nonexistent** `FRESH_OUT`. Verify all expected hashes in identity.json; toolchain/build-date changes can change a rebuilt binary hash and must be recorded, not silently treated as the frozen artifact.

```sh
SOURCE_SHA=dbff5b09d35a00a07572751d556b330a9da38acd
git fetch origin main
git worktree add --detach "$SOURCE" "$SOURCE_SHA"
CARGO_REAL="$(rustup which cargo)"
CARGO_TARGET_DIR="$BUILD_DIR" RUSTC_WRAPPER= RUSTC_WORKSPACE_WRAPPER= \
  "$CARGO_REAL" build --manifest-path "$SOURCE/Cargo.toml" --locked --profile dev-release -p pixel-cli
BINARY="$BUILD_DIR/dev-release/pixel"
"$BINARY" --version
python3 -I "$SOURCE/scripts/bench/tool-census/score_brief.py" freeze-home \
  --repo "$SOURCE" --commit "$SOURCE_SHA" --out "$PROBE_HOME"
python3 "$REPORT_DIR/run.py" --source "$SOURCE" --binary "$BINARY" --home "$PROBE_HOME" \
  --state "$STATE" --out "$FRESH_OUT"
```

`run.py` takes `/tmp/pixel-benchmark.lock` and runs this exact logical argv (exit 0; label base/head denotes **candidate repetitions**, not the historical reference):

```sh
python3 -I "$SOURCE/scripts/bench/tool-census/score_brief.py" pinned \
  --base "$BINARY" --head "$BINARY" --probe-home "$PROBE_HOME" \
  --questions "$SOURCE/eval/brief-bench/questions.jsonl" --route hook \
  --root "$STATE" --out "$FRESH_OUT" --label integration
```

[commands.json](commands.json) records build/measurement argv and exits. Exact expanded local argv and raw reports/briefs are retained in the local evidence bundle; its command digest is recorded in identity.json. [compare.py](compare.py) recomputes the question/category aggregates with the pinned scorer’s `aggregate`, compares statuses and all non-latency metrics, and retains abstentions. Use a fresh nonexistent `COMPARISON_OUT`:

```sh
python3 "$REPORT_DIR/compare.py" --source "$SOURCE" \
  --reference "$SOURCE/docs/bench/scorer-protocol-v4-2026-10-19" \
  --pair "$FRESH_OUT" --out "$COMPARISON_OUT"
```

The comparison exited 0; its three generated files reproduce the committed per-question, per-category and summary records exactly. JSON parsing, Python compilation, SPDX and whitespace checks also passed. Published JSONL keeps metric rows, metadata and hashes without committing raw brief text or personal machine paths. Raw reports/briefs, expanded argv, build logs, accepted binary and frozen probe HOME are retained locally in `906-integration-2026-10-19/` beside the original `1201-audit/` artifact; they are not part of the public repository.

## Limits and later live controls

No real agent, paired final answer, call-count reduction, semantic quality approval or new live campaign ran. The development set is not held out; two deterministic native repeats do not prove live stability. Shared model/cache contents, PATH tools and OS are not fully pinned by #1201. Numbers describe this frozen integration only and do not attribute causality among the four merged units. No retrieval/scorer/probe code was changed in this report PR; historical report files remain byte-identical.

For a future separately authorized paired quality check, prioritize **flask-04** (preserve redirect/error evidence outside the oracle), **flask-07** (global definition vs actual unbound-proxy trigger), **flask-03** (conditional file loading without invented mode/precedence), **requests-07** (dependency message vs local conversion). Use **pixel-02 / fd-02** as pure-code negative controls for #1234. ripgrep-07 is a useful optional verbosity/conditions check. Keep model/effort fixed across arms and follow the published live repetition/answer-review protocol before any zero-call claim.
