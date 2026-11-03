# Common scorer reference — protocol v4, 2026-10-19

Follow-up #1239 / PR #1240 to the two remaining v3 source-ownership defects.
This is a measurement correction. No retrieval or zero-call gain is claimed.
The [dated v3 reference](../scorer-protocol-2026-10-18/README.md) and historical reports are preserved.

## Identity

- Scorer code commit: `8c02189cc2d134d008b80ddc1672bd6d94e13f8a`; later evidence-only commits keep the same scorer bytes.
- Scorer SHA-256: `95bf349407a86091b7627f0d7726697478408b743345b32d4ed919da36233106`.
- Baseline: merged v3 `20670993b0f2ed778cd7226955e4582df91c37e0`, SHA-256 `febb6031075e4f20f3e2380451ce7b88cbb7613b89ae904252987dc756d3889e`. The actual replay checkout was `3027ea60f4c2692905828860218617e1d70f7f46`, whose complete tree is identical to that merged baseline; identity.json records both commits.
- Binary source `a0d99d6268b770b49bf44effef415ac95b1252aa`, dev-release, SHA-256 `fcde2a41d4d0e7017acdba2ee58e4803afa9379b8b3995b68e24b71ec14d8e64`.
- Same 14 programs + 14 manifests frozen at `a0d99d62`; all hashes and seven verified repository pins are in [identity.json](identity.json).
- Same 56 questions, SHA-256 `984a632e96f5f32891279d58f872c3a891bfeabaee110b3993be313c63e17069`.
- Hook Pi brief_only, warm, 3 parts / 3000 ms, dedupe off, process timeout 300 s; serialized with `/tmp/pixel-benchmark.lock`.

## Before / after

Two new regression methods on v3 fail with six assertions: an unknown source borrows code, mixed fences invent an in-range coordinate, and valid mixed bare rows are lost. On v4 both methods pass; genuine headerless and wholly unnumbered excerpts retain their positive controls. The full scorer passes 40 tests; brief-check 16, probe-test 24, probe-translate 4. SPDX and whitespace checks pass.

The transmitted external controls were replayed unchanged in a separate local output directory: **31/34 on v3 → 34/34 on v4**, including 22/22 process controls in both. That replay by the implementer is distinct from independent review. The original reviewer subsequently retested exact code SHA `8c02189c` independently: **34/34 controls, 3/3 minimal controls and all 40 scorer self-tests pass**; the 56-brief replay also has zero question/category deltas. A native optional-probe timeout preserves valid evidence, excludes expired evidence and leaves the brief/scorer `ok`. The frozen external control script SHA-256 and report SHA-256 are recorded in identity.json; full raw evidence is retained in the local audit bundle. This is external audit evidence, not a GitHub approval or merge authorization.

The same 56 frozen briefs yield **0 changed questions and 0 changed categories**, exits 0/0. These corner cases are outside that corpus. Exact values and zero deltas: [per-question.jsonl](per-question.jsonl), [per-category.jsonl](per-category.jsonl).

| Metric | v3 | v4 |
|---|---:|---:|
| file_recall | 0.425378 | 0.425378 |
| region_cov | 0.084869 | 0.084869 |
| line_cov | 0.110799 | 0.110799 |
| inline_cov | 0.093815 | 0.093815 |

Two full native #1201 passes with the unchanged binary/frozen home reproduce **56/56 byte-identical briefs**, also matching the old frozen texts. No metric differs between native passes or from the v4 replay. Both reports are complete/exit 0: **53 ok + 3 abstained**, no errors, cuts or partial results. Abstentions remain in the denominator. Internal p95 latency is 657 / 705 ms (descriptive, equality is not required). Per-row evidence: [native-repeat.jsonl](native-repeat.jsonl).

## Commands to reuse

Use Git worktrees for the baseline/candidate revisions above; verify their hashes and the fixed inputs against identity.json. Set `SCORER`, `V3_SCORER`, `BINARY`, `PROBE_HOME`, `QUESTIONS`, `FROZEN_REPORT`, `FROZEN_BRIEFS`, a dedicated `ROOT`, and fresh output labels/directories. Expanded argv are retained in the local evidence bundle; their SHA-256 is in identity.json.

```sh
python3 -I scripts/bench/tool-census/replay_briefs.py \
  --before "$V3_SCORER" --after "$SCORER" \
  --report "$FROZEN_REPORT" --briefs "$FROZEN_BRIEFS" \
  --binary "$BINARY" --probe-home "$PROBE_HOME" --questions "$QUESTIONS" \
  --root "$ROOT" --out "$FRESH_REPLAY_OUTPUT"

python3 -c 'import fcntl,subprocess,sys; lock=open("/tmp/pixel-benchmark.lock","w"); fcntl.flock(lock,fcntl.LOCK_EX); sys.exit(subprocess.call(sys.argv[1:]))' \
  python3 -I "$SCORER" pinned --base "$BINARY" --head "$BINARY" \
  --probe-home "$PROBE_HOME" --questions "$QUESTIONS" --route hook \
  --root "$ROOT" --label "$FRESH_RUN_LABEL"
```

The replay takes the common lock itself and substitutes frozen execution results; it measures scoring only. The separate native pair verifies the full execution path. For a content candidate, hold the binary fixed and replace `--head "$BINARY"` with `--head-probe-commit "$CANDIDATE_REV" --repo "$CONTENT_REPO"`. Change only that input.

## Limits

No real-agent run, semantic answer-quality review, or zero-call claim. Models/cache contents, PATH tool versions and OS are not fully frozen by #1201. Passing this corpus does not establish absence of other parser edge cases. CI and independent review must identify the exact PR head; an evidence-only commit can share the measured scorer hash.
