# Codex retrieval arena

Seven containerized arms compare retrieval tools against native Codex: `raw`,
`semble`, `graft`, `stacklit`, `gitnexus`, `gortex`, and `pixel`. Each arm uses
its own writable repository snapshot; each repetition gets a fresh snapshot.
Authentication is mounted at runtime, never baked into an image.

Build the shared base before the first run:

```bash
rtk docker build -f eval/arena/Dockerfile.base -t pixel-arena-base:latest eval/arena
```

## Tiny paired loops

Start with one scenario, one repetition and two arms. Use a new output directory
for every run so an earlier failure or a different candidate cannot enter its
comparison.

```bash
REPO_SNAPSHOT=/path/to/foreign/repository \
PIXEL_SRC=local \
CODEX_MODEL=gpt-5.6-terra CODEX_EFFORT=medium \
rtk bash eval/arena.sh --arms "raw pixel" --tasks "g1-locale-routing" \
  --reps 1 --assert-context-parity \
  --results-dir eval/arena-results/g1-candidate-01
```

`REPO_SNAPSHOT` supplies the repository being investigated. Local Pixel source
comes from the repository containing this script, or `PIXEL_SRC_DIR` when set.
Use the same repository revision, model, effort and scenario for both arms.
The `g*` scenarios target the foreign example application; the `s*`
scenarios target Pixel itself. A scenario's expected paths must exist in the
chosen repository.

Inspect the transcripts as well as the rank table. A required-pattern score
measures answer coverage, not semantic correctness. Token totals include
reported input and generated tokens; cached input is part of input and must not
be counted twice. Unknown usage is unknown, not zero. Failure counts belong
beside completed-pair measurements; a transcript containing an answer does not
by itself explain a nonzero container exit.

Keep the first loop diagnostic. A single pair can expose a forced extra lookup
or an incorrect answer, but cannot establish a general speedup. Confirm useful
changes on another generic question and on the structural question meant to
benefit. Preserve the source identity, image identity, complete logs and exit
statuses with each result. Change one input between candidate comparisons.
