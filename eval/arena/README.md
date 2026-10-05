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

To reuse a particular candidate, set `PIXEL_IMAGE_SOURCE=existing` and
`PIXEL_ARENA_IMAGE=sha256:<image-id>`. The runner resolves every selected image
once, checks the actual container image before starting it, and saves that
receipt beside the results. It also refuses comparisons across different Codex
versions. Local builds cache Cargo's registry, Git dependencies, and build
artifacts; the first build still needs to populate those caches. Git builds use
the resolved commit as Cargo's actual `--rev`.
The runner bind-mounts its current entrypoint, context manifest and hook auditor
into each container, including when reusing a pinned image; harness-only edits
do not require an image rebuild. `harness-source.json` records SHA-256 values
and container paths for the runner files used in that run.

Graph experiments use `--prepare-pixel-graph`; the saved setup receipt separates
index preparation from model time. Retrieval routing stays native by default,
including when a graph is prepared. A fresh Codex home may leave installed
hooks untrusted: static context parity alone cannot establish hook delivery.
`--assert-context-parity` additionally expects no Pixel calls and is intended
for generic-question abstention controls.

### Explicit reviewed-hook experiment

The default run keeps Pixel retrieval routing native and does not bypass Codex
hook trust. For a disposable raw/Pixel pair that intentionally exercises
Pixel's Codex prompt hook, add `--review-pixel-hooks`. This flag only audits and
records delivery; it does not enable experimental caller facts. Both containers
must finish the audit before either model starts: raw must have no hooks; Pixel
must have exactly the 11 known Pixel Codex commands under
`/root/.codex/hooks.json`; project/global foreign hooks and plugin hook
declarations stop the pair. Only after both audits pass does the runner apply
the same Codex hook-trust bypass to both arms. It does not change host hook
trust. The Pixel prompt-hook wrapper records its validated `UserPromptSubmit`
response, emitted context status and hook stderr in
`pixel-hook-<rep>.jsonl`; task stderr is retained as
`<arm>-<task>-<rep>.stderr`. Neither receipt stores prompt input or auth. A
receipt proves only that context text was returned and forwarded by the hook;
it does not prove the model used it. Reviewed-hook mode is limited to one task
per run so the receipt corresponds to the selected scenario; the harness marks
the Pixel result failed if that task produces no valid hook response.

Experimental indexed-caller facts require a separate explicit flag. This also
requires graph preparation and reviewed hooks, and sets the installed Pixel
hook's `PIXEL_CODEX_CALLER_FACTS=1` switch only in the Pixel container:

```bash
REPO_SNAPSHOT=/path/to/foreign/repository \
PIXEL_IMAGE_SOURCE=existing PIXEL_ARENA_IMAGE=sha256:<pinned-image-id> \
CODEX_MODEL=gpt-5.6-terra CODEX_EFFORT=medium \
rtk bash eval/arena.sh --arms "raw pixel" --tasks "g4-transfer-callers" \
  --reps 1 --prepare-pixel-graph --review-pixel-hooks --codex-caller-facts \
  --results-dir eval/arena-results/g4-reviewed-hooks-01
```

Use a new results directory and the same pinned image/repository/model for each
paired comparison. The flags are opt-in; caller facts are not enabled by
`--review-pixel-hooks` alone. Compare the transcript and answer quality as
well.

The hook allowlist intentionally matches the current Pixel Codex installer.
If its command set or event names change, update the allowlist and tests before
using this reviewed-hook mode.

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
