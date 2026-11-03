# Brief staleness: public 56-question diagnostic (2026-10-20)

Issues #1227, #1267 and #1268. This is a deterministic oracle-coverage and latency comparison on the public seven-repository set. It is not a live-agent quality review, a zero-call acceptance result, or the gumroad after-edit campaign (#1228).

## Frozen inputs

- Baseline: `c6cd78edf42272d401295935174862d6a069dd09`.
- Candidate: `c54943f9f750c4e62b04bf12802b803d8b708d19`.
- Same question file, repository pins, frozen probes from baseline c6cd78ed, warm semantic indexes and prompt-submit hook route (3 s window and probe floor). No compilation ran concurrently with this final pair. Each binary runs the 56 questions once, sequentially; latency remains a single-run diagnostic, not a statistically established speedup.
- `identity.json` carries binary/dataset/probe hashes, scorer identity, parameters and aggregate results. `questions.jsonl` carries per-question pins and measurements. The earlier exploratory baseline is excluded.

## Results

| Measure | Baseline | Candidate |
|---|---:|---:|
| Usable briefs | 56/56 | 56/56 |
| Mean oracle-line coverage | 13.06% | 13.06% |
| Mean inline coverage | 10.74% | 10.74% |
| Mean file recall | 45.75% | 45.75% |
| p95 elapsed (ms) | 1084 | 1076 |
| Maximum elapsed (ms) | 2629 | 2606 |
| Partial briefs | 0 | 0 |

All **56/56 rendered briefs are byte-identical**. `output-hashes.jsonl` records
both SHA-256 values for each question. Verification: `diff -rq
"$RUN/final-scores/issue1227-base/briefs" "$RUN/final-scores/issue1227-head/briefs"`
exits 0 with no differences; both directories contain 56 `.txt` files.

The benchmark uses fresh prepared repositories. It checks the normal route; the stale-after-edit contract is established separately by held-batch daemon tests and the socket/brief regression below. Coverage is diagnostic and does not establish answer correctness.

## Reproduction

Build each source revision with `cargo build --profile dev-release -p pixel-cli` and save the binaries as `$RUN/base-pixel` and `$RUN/head-pixel`. Then:

```sh
python3 -I scripts/bench/tool-census/score_brief.py freeze-home --commit c6cd78ed --out "$RUN/probe-home"
python3 -I scripts/bench/tool-census/score_brief.py pinned --base "$RUN/base-pixel" --head "$RUN/head-pixel" --probe-home "$RUN/probe-home" --root "$RUN/bench" --out "$RUN/final-scores" --label issue1227
```

## Regression evidence

- Before Rust edits, a scratch Python/socket prototype exposed a daemon definition that the baseline brief refused locally (`find-symbol login: the graph is not built`, zero Symbol requests). The proposed reader fetched that definition and emitted the staleness notice, including for a 0 ms lag.
- `cargo test --profile dev-release -p pixel-daemon --lib stale_brief_`: 9 passed. Temporarily restore the pre-change `Op::stale_ok` match: 0 passed, 9 failed. Write-plane reads fail at `stale_ok graph read must reach the replica`; read-plane operations fail because their response lacks the expected `Some(1500)` lag and generation warning. Restore the fix: 9 passed.
- `cargo test --profile dev-release -p pixel-cli --bin pixel brief_should_render_answered_daemon`: restoring the local graph refusal fails at `daemon definition must survive`; separately removing the response staleness mark fails the notice assertion. Restoring both passes. Fresh daemon and local-only behavior have separate controls.
- Replaying the same socket prototype with the native candidate makes one Symbol request (baseline: zero), renders the definition in a region pack and retains the staleness notice.
- The source-hash protection remains: Context omits changed source rather than quoting new text against old graph spans. Search scans current file contents using published candidates; a generation label is not a promise of historical source bytes.
- No local installation or mutation campaign; no live-agent or gumroad rerun. Full workspace and platform validation belong to the PR head CI.

## Per question

| Question | Line coverage before | After | Elapsed before (ms) | After |
|---|---:|---:|---:|---:|
| pixel-01 | 15.69% | 15.69% | 230 | 228 |
| pixel-02 | 17.28% | 17.28% | 595 | 547 |
| pixel-03 | 2.17% | 2.17% | 986 | 1018 |
| pixel-04 | 2.87% | 2.87% | 1282 | 1245 |
| pixel-05 | 0.00% | 0.00% | 337 | 304 |
| pixel-06 | 0.00% | 0.00% | 507 | 475 |
| pixel-07 | 0.00% | 0.00% | 1084 | 1076 |
| pixel-08 | 1.34% | 1.34% | 298 | 285 |
| ripgrep-01 | 15.31% | 15.31% | 122 | 112 |
| ripgrep-02 | 9.22% | 9.22% | 387 | 325 |
| ripgrep-03 | 24.59% | 24.59% | 359 | 309 |
| ripgrep-04 | 6.11% | 6.11% | 497 | 438 |
| ripgrep-05 | 0.00% | 0.00% | 217 | 188 |
| ripgrep-06 | 1.10% | 1.10% | 166 | 144 |
| ripgrep-07 | 10.12% | 10.12% | 529 | 562 |
| ripgrep-08 | 20.22% | 20.22% | 529 | 452 |
| requests-01 | 2.54% | 2.54% | 173 | 130 |
| requests-02 | 18.96% | 18.96% | 312 | 290 |
| requests-03 | 36.54% | 36.54% | 295 | 267 |
| requests-04 | 35.94% | 35.94% | 523 | 523 |
| requests-05 | 66.67% | 66.67% | 232 | 254 |
| requests-06 | 3.54% | 3.54% | 112 | 128 |
| requests-07 | 20.51% | 20.51% | 323 | 335 |
| requests-08 | 4.62% | 4.62% | 170 | 185 |
| excalidraw-01 | 9.85% | 9.85% | 166 | 164 |
| excalidraw-02 | 3.57% | 3.57% | 500 | 469 |
| excalidraw-03 | 6.48% | 6.48% | 453 | 428 |
| excalidraw-04 | 2.85% | 2.85% | 523 | 552 |
| excalidraw-05 | 0.62% | 0.62% | 308 | 293 |
| excalidraw-06 | 12.47% | 12.47% | 250 | 261 |
| excalidraw-07 | 10.64% | 10.64% | 673 | 615 |
| excalidraw-08 | 0.00% | 0.00% | 2629 | 2606 |
| flask-01 | 17.09% | 17.09% | 108 | 98 |
| flask-02 | 11.29% | 11.29% | 181 | 173 |
| flask-03 | 24.70% | 24.70% | 217 | 239 |
| flask-04 | 54.62% | 54.62% | 550 | 535 |
| flask-05 | 0.00% | 0.00% | 225 | 222 |
| flask-06 | 8.60% | 8.60% | 134 | 133 |
| flask-07 | 4.67% | 4.67% | 265 | 327 |
| flask-08 | 7.29% | 7.29% | 153 | 175 |
| fd-01 | 13.08% | 13.08% | 93 | 100 |
| fd-02 | 44.74% | 44.74% | 171 | 175 |
| fd-03 | 11.53% | 11.53% | 190 | 203 |
| fd-04 | 16.94% | 16.94% | 343 | 336 |
| fd-05 | 16.67% | 16.67% | 164 | 176 |
| fd-06 | 6.50% | 6.50% | 110 | 87 |
| fd-07 | 38.97% | 38.97% | 192 | 205 |
| fd-08 | 13.33% | 13.33% | 114 | 104 |
| express-01 | 0.51% | 0.51% | 118 | 119 |
| express-02 | 1.01% | 1.01% | 193 | 186 |
| express-03 | 1.76% | 1.76% | 195 | 177 |
| express-04 | 25.36% | 25.36% | 404 | 357 |
| express-05 | 0.00% | 0.00% | 218 | 192 |
| express-06 | 12.46% | 12.46% | 121 | 114 |
| express-07 | 8.80% | 8.80% | 259 | 245 |
| express-08 | 29.81% | 29.81% | 133 | 122 |
