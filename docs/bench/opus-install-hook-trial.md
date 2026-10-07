# Opus install-hook trial

## Result

Eleven runs per arm on the same low-research scoping task, with Claude Opus
5.5 at medium effort. The comparison was without Pixel versus Pixel's
`pixel install` hooks. Medians are:

| Metric | Without Pixel | With Pixel hooks | Change |
| --- | ---: | ---: | ---: |
| Duration | 42.9 s | 47.6 s | +11% |
| Tokens read | 11,945 | 13,350 | +12% |
| Provider-reported API cost | $0.283 | $0.372 | +31% |
| Pixel calls per run | 0 | 0–4 observed | — |

The trial therefore found higher duration, read volume and provider-reported
cost with hooks on this task. Pixel was called at most four times in a run;
the range is not a median. This is evidence about one task and setup, not a
general claim that Pixel slows agents or increases their costs. It does not
show that every task benefits from repository context.

## Reproducing the run

The 22 runs were recorded with the repository's own demo recorder, at the
`v0.5.0` tag, in a clean worktree, with the same isolation `record-demo.sh`
gives every run (its own copy of a fresh repository holding only that ref's
history, its own daemon and `PIXEL_SESSION_ID`):

```bash
cd docs/motion
REF=v0.5.0 EFFORT=medium scripts/record-demo.sh <out-dir> 11 claude-opus-5-5
```

The command is the provenance of the medians above: model `claude-opus-5-5`,
effort `medium`, 11 reps per arm, arms `vanilla` (no Pixel) and `pixel` (the
hooks `pixel install` writes). Re-running it reproduces the run identity; the
medians are a property of the model's responses on that task, not of the
script.

## What is established

- The 22 runs were completed at the `v0.5.0` tag in a clean worktree.
- All 22 responses were in English, and none read the demo files.
- The model, effort, task, arms, sample counts and medians above were reported
  for this trial.
- Costs are provider-reported, not independently reconciled against invoices.

## Provenance limits

The raw run bundle is not archived in this repository. The exact trial date,
per-run records and distributions, exact CLI/harness versions beyond the
`v0.5.0` worktree tag, and the token-counting method are unavailable here.
Consequently, these medians cannot be independently recomputed from the
repository, and the reported token counts should not be treated as billed
provider tokens. No confidence interval or significance test is available.
