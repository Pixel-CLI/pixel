# Configuration declaration identities — #1233, 2026-10-19

This unit preserves homonymous declarations as separate `key @ path:line` candidates with bounded scope context. It does not resolve lexical bindings or establish a loader/mode precedence. #1199 remains CLOSED / COMPLETED; full binding/loader work remains in #1233.

## Hypothesis and prototype

Hypothesis: retaining every homonymous declaration under its source identity prevents an unrelated module’s default from becoming the unique answer, while preserving the evidence relevant to the named consumer.

A scratch prototype outside crates tested the existing two-module fixture: both declare DEFAULT_TIMEOUT, with values 1101 (alpha) and 2202 (beta). The old registry returned only alpha. The prototype and shipped regression now quote both declaration identities and their scope context, with binding/precedence explicitly unknown. The targeted beta oracle improves **0/2 → 2/2 lines**; the alpha positive remains 1/1 and an unrelated tour request stays empty. These are synthetic correctness controls, not public-benchmark gains.

`python3 -I probes/config-registry/tests/test_probe.py` passes **13 tests**. With `PROBE_UNDER_TEST` pointing to the frozen baseline program, the same suite has **7 failures**, including the missing beta declaration. Two different scopes in one file retain distinct coordinates too. Other controls cover a unique constant, four-candidate cap/omission count, unresolved env-mode candidates, tracked/untracked templates, tracked symlinks, scan-line bounds and masked values/context.

The initial input-lane prototype admitted env lines beyond the existing scan bound. Review restored the 300-byte line limit (including UTF-8) and shared file-size bound with named constants, then reran the final native pair on those corrected bytes. The earlier exploratory pair is not used below. No retrieval hypothesis was dropped or replaced; `config-sites` remains the previously rejected, separate hypothesis and was not reimplemented.

## Frozen comparison

Uses the common corrected **v4** reference following #1230/#1235/#1239. The scorer is unchanged. Full identities, 28 probe hashes per arm, seven repository pins and expanded argv are in [identity.json](identity.json).

- Binary source `a0d99d6268b770b49bf44effef415ac95b1252aa`, SHA-256 `fcde2a41d4d0e7017acdba2ee58e4803afa9379b8b3995b68e24b71ec14d8e64`; same binary in both arms, no rebuild or installation.
- Scorer v4 SHA-256 `95bf349407a86091b7627f0d7726697478408b743345b32d4ed919da36233106` from main `de1563de6d1190936e26bd45865d15f8d30889d7`.
- Questions SHA-256 `984a632e96f5f32891279d58f872c3a891bfeabaee110b3993be313c63e17069`, 56 questions / seven pinned public repositories.
- Baseline probes `a0d99d6268b770b49bf44effef415ac95b1252aa`; measurement-only candidate `6ddc7c7e539ab5fdaae13a08e5db01dad10a3f07`. Exactly **one of 28 files changes**, `config-registry/probe`, whose SHA-256 is `18202dad4d6e259a9af8f87606353db9cc6031cbb05230f5e1924e27c32e2109` and matches this PR’s program.
- A temporary Git index overlays that one program on the frozen baseline tree. This isolates unrelated probe changes already merged on main. The real index/branch are untouched; the overlay tree and program Git blob are recorded for reconstruction. The object is a local measurement snapshot, not a separate PR unit.
- Both arms: hook Pi brief_only, warm, three parts / 3000 ms, process timeout 300 s, dedupe disabled; private clones and `/tmp/pixel-benchmark.lock`. No other active scorer observed at lock acquisition.

Exact invocation from the dedicated content worktree, while holding that lock (exit **0**):

```sh
/Users/navid/.local/share/mise/installs/python/3.15.0/bin/python3 -I scripts/bench/tool-census/score_brief.py pinned --base /tmp/pixel-1233-homonyms/pixel --head /tmp/pixel-1233-homonyms/pixel --probe-commit a0d99d6268b770b49bf44effef415ac95b1252aa --head-probe-commit 6ddc7c7e539ab5fdaae13a08e5db01dad10a3f07 --repo /Users/navid/.codex/worktrees/906-1199-followup/pixel --questions /Users/navid/.codex/worktrees/906-1199-followup/pixel/eval/brief-bench/questions.jsonl --label homonyms56-final --route hook --root /tmp/pixel-1233-homonyms/state --out /tmp/pixel-1233-homonyms/byte-runs
```

For a fresh reproduction, load the baseline tree with `git read-tree` under a temporary `GIT_INDEX_FILE`, replace only `probes/config-registry/probe` with this revision’s recorded blob, then `git write-tree` / `git commit-tree -p <baseline>`. Pass that overlay commit as `--head-probe-commit`; its program/manifest hashes must match identity.json. The permanent source tree includes unrelated main changes, so passing its whole probe bundle would change a second input.

| Metric | Baseline | Candidate | Delta |
|---|---:|---:|---:|
| Config oracle type, across 19 questions | 225/979 (22.9826%) | same | 0 |
| Config category line_cov, 7 questions | 0.100974142 | 0.100974142 | 0 |
| Config category region_cov | 0.085233228 | 0.085233228 | 0 |
| All file_recall | 0.425377968 | 0.425377968 | 0 |
| All region_cov | 0.084869149 | 0.084869149 | 0 |
| All line_cov / inline_cov | 0.110799104 / 0.093814809 | same | 0 |

**0/56 question coverage deltas, 0 category deltas, no measured coverage regression.** Detailed before/after values and deltas: [per-question.jsonl](per-question.jsonl), [per-category.jsonl](per-category.jsonl). The baseline exactly reproduces the corrected v4 reference. Legacy 371/979 config figures used the old scorer and are not this comparison’s baseline.

Both reports are complete/exit 0: 53 ok + 3 abstained, no errors, cuts or partial rows. Abstentions remain in the denominator. Probe bytes and all pins were independently checked against Git objects. Text changed only for ripgrep-03, excalidraw-03; unchanged metrics do not imply byte-identical briefs or equal answers. Sensitive source context is conservatively masked. No performance conclusion is drawn from the timings.

## Checks and limits

`python3 -I scripts/bench/probe-test.py --check-all` passes its contract/determinism/read-only checks. Its 24 self-tests pass; the fixture overfit guard finds no hard-coded prompt/path literals in the executable. Python compilation, SPDX and whitespace checks pass. CI validates the published PR head; Rust gates and installed-hook deployment were not run for this probe-only change.

No live agent run, semantic review of paired agent answers, tool-call reduction or zero-call claim. Scope is preserved as located source context, not resolved bindings. Only four declaration candidates per key are quoted, with omissions explicit. Full loader/mode precedence, imports, distant usages and multi-line declarations remain unresolved. `excalidraw-03` remains at zero measured coverage; this unit does not fix that independent weakness.

The existing `probe-test.py` evaluator also passes the three frozen synthetic
cases through its executable/budget/determinism/read-only/merge checks:
`PROBE config-registry | beta,positive | predicted calls 1 -> 0 | coverage 50% -> 100% | OK`.
That offline verdict is `OK`, not a live `KEEP`; its synthetic residual-call
placeholders measure oracle completeness, not observed tool use. The native
56-question pair is the authoritative replacement comparison for the existing
probe, rather than appending a second copy to an installed-home brief.

Verdict: **proved improvement on the bounded homonym contract; no public-56 coverage gain and no measured coverage regression**.
