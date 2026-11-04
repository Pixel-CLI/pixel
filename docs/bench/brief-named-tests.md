# Named test evidence in the prompt brief

Run date: 2026-11-04. This experiment evaluates whether a test inventory that already names relevant tests can make their source bodies survive the test-body probe's lexical cutoff. It does not measure competitor performance.

## Run identity

The baseline is upstream `main` at `9dd036279766e1454a135543e3c1566f9fbc97ea`. The candidate is that base plus the four-file implementation/test diff with SHA-256 `5eb203e929c599cceade26c2958ef5fd879a7f949da03c457de92efb7109d595`. Both native binaries use the `dev-release` profile on macOS arm64 with rustc 1.97.1. The uncommitted candidate reports the same base commit in `--version`; the source and binary hashes distinguish it.

To verify the source identity from this branch:

```sh
rtk proxy git diff --binary 9dd036279766e1454a135543e3c1566f9fbc97ea -- crates/pixel/src/execution_brief/steps/test_bodies.rs crates/pixel/src/execution_brief/steps/test_bodies/tests.rs probes/test-bodies/probe probes/test-bodies/tests/test_probe.py | rtk proxy shasum -a 256
```

The local live-run script is `/tmp/pixel-brief-evidence-20261104/run-final-live.py` (SHA-256 `5b04726cedc16022c995000842711694b6dfacd005a6111bb5053666ec6cb187`). The script and raw model receipts remain local; the commands, fingerprints and reviewed results are recorded here.

## Change

The `test-bodies` probe now recognizes inventory entries by source path, declaration line and full test title. A listed test may survive the lexical relevance cutoff; the normal score cutoff still uses the strongest score across all candidates, so a listed weak match cannot lower the threshold for unrelated tests. Inventory matching does not bypass explicit `--exec`/`--exec-batch` filtering. Rust and Python oracle tests cover arbitrary Rust names, multiword JavaScript titles with a spaced em dash, duplicate names on distinct lines, and threshold preservation.

## Offline guard

Command, run against the final source binary (`/tmp/pixel-brief-candidate-target/dev-release/pixel`, SHA-256 `deea0ae8758c750009345712b6b6d3513832085621c924140039ee647df6059e`):

```sh
rtk proxy python3 -I scripts/bench/probe-test.py test-bodies --bin /tmp/pixel-brief-candidate-target/dev-release/pixel --report-dir /tmp/pixel-brief-evidence-20261104/final-oracle-emdash
```

The seven existing public `*-06` test questions passed the oracle guard. Predicted aggregate tool calls changed 27→26; oracle source-line coverage changed 14%→20%. This is offline probe scoring, not a live tool-call reduction claim. Report: `/tmp/pixel-brief-evidence-20261104/final-oracle-emdash/report.md`.

## Live Codex evidence

Three paired Codex sessions used the same prompt and frozen Requests checkout: `where are the tests for redirect handling and what scenarios do they cover`; repo commit `611c6162cbc4ac2020a2f91c7cfa4f3abf9bbb60`; model `gpt-5.6-terra`, medium effort; three injected brief parts, each bounded by 9,800 bytes. The harness prepared each clone with `prepare-repo --no-daemon`, ran `search-meaning`, disabled daemon autostart, and settled the brief before the session. It used the project's `scripts/bench/brief-bench.py::run_codex` path and `brief-zero-calls.py` Codex-home setup. Command: `rtk proxy python3 -I /tmp/pixel-brief-evidence-20261104/run-final-live.py`; final source binary SHA-256 `deea0ae8758c750009345712b6b6d3513832085621c924140039ee647df6059e`; baseline binary SHA-256 `386528a32cd9007eb2738204c624f1d48f6d62e6af9f1571726466ea0217b00e`. A transparent wrapper recorded the exact stdout bytes of each prompt-submit part, then forwarded stdout/stderr/exit status unchanged. Receipt with all six answers: `/tmp/pixel-brief-evidence-20261104/final-live-3pairs/records.json`; all 18 raw hook receipts and hashes: `/tmp/pixel-brief-evidence-20261104/final-live-3pairs/hook-stdout-final/`.

| Pair | Baseline calls | Candidate calls | Baseline brief bytes | Candidate brief bytes |
|---|---:|---:|---:|---:|
| 1 | 0 | 0 | 11,854 | 18,780 |
| 2 | 0 | 0 | 11,854 | 18,780 |
| 3 | 0 | 0 | 11,854 | 18,780 |

The source-checked behavior result reproduces across all three final-source pairs: every baseline describes `WITH_PARAMS` as URL/query parameters, while each candidate correctly identifies a custom redirect limit. The test sets `Session.max_redirects = 5` and asserts the exception request/response URLs and five redirect-history entries. Candidates also describe the separate custom `get_redirect_target` mixin behavior, though their citations do not consistently point directly to that test. This judgment comes from reviewing all six answer texts against the frozen tests, not from citation counts. Candidates used 6,926 more brief bytes per run across the three parts (58.4% of the baseline packet); both arms made zero calls, so this experiment shows no call reduction.

Citation precision is mixed: one candidate uses absolute paths into the temporary clone, another uses broad ranges or omits a direct citation for the custom mixin test, and one baseline includes unrelated `test_encoded_methods` as redirect coverage. Both pair-3 answers suggest `pytest -k redirect`, which omits some method-rewriting tests and should not be read as a complete focused suite command. These limits do not change the observed `WITH_PARAMS` interpretation improvement.

The exact delivered text confirms the added evidence is in part 2. For each arm, part 1 was 9,775 context bytes (SHA-256 `0d6207ec1ffec17eafcf8c42d2941d8c6e360f9fbf68a628dff6fda06176bf21`); part 2 was 2,079 bytes baseline (SHA-256 `509005ee111726ddb00b4cec9a1561ae1ed805362ca8913daf4bcf3144f9497d`) and 9,005 bytes candidate (SHA-256 `d1f161b6f7cab28fdaa2d721fd1e4d44ad3367f1a781c7298677dc54ac3f1d96`); part 3 was empty in both arms. The shared part 1 and candidate part 2 hashes were identical across the three repetitions.

## Verification

The final source includes the parser fix for valid JavaScript test titles containing a spaced em dash. The final native binary's exact delivered hook packets above were captured during the three paired sessions, so the measured retrieval delta belongs to the final source build. The final source passed 25 focused Rust tests, 18 Python oracle tests and the seven-question offline guard. Offline predicted calls changed 27→26 and oracle source-line coverage changed 14%→20%; this remains a separate offline measurement, not a live call-reduction claim.
