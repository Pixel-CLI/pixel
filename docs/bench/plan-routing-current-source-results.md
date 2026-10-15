# Current-source replay: routing recommendation unchanged

Executed 2026-09-22 after the user requested a source-built comparison instead
of the installed 0.4.0 baseline. **Keep the learned router experimental.** The
fresh source binary returned the same query-name sets on all 100 frozen requests.
This resolves the runtime-version uncertainty for this routing pilot; it does
not benchmark improvements to file ranking, graph correctness or other commands.

## Source and runtime identity

- Checkout: `experiment/coding-routing-pilot`, HEAD
  `5c894b1b3a5db713615d906dc8604805c73813ae`. Existing experiment files preserved.
- Fresh `pixel fetch origin` confirmed `origin/main` at
  `f7ae9d044b1619ca5beeb9d14d0f2a29477500fa`. The checkout differs from that
  main in docs/benchmark scripts only; no Rust/Cargo source differences. No reset,
  merge, rebase, global installation or source edit was performed.
- Executed `CARGO_BUILD_JOBS=4 cargo build --release --locked -p pixel-cli
  --message-format=json`; success, **80.78 seconds**, process peak RSS
  **1,437,941,760 bytes**. Cargo reports executable artifact `fresh: false` and
  features `default`, `fastembed`, `model2vec`. About 23 GiB free before build.
- Built binary reports version **0.4.0**, but also embeds the actual source
  commit `5c894b1...`, target `aarch64-apple-darwin`, rustc **1.98.1** and build
  date. The unchanged package version alone cannot identify an installed release.
- Copied the build artifact to the run's `pixel-current`, SHA256
  `c866661ca42c19ac2424d2a8e93d90a6f494283c2c3784bd1768c2bbf482e8a7`.
  Installed release hash was
  `b0c3755ba63e35d8188d61a6aac3c788504ff69538d0b36e53b5c10c8de2248d`;
  its `--version` identifies commit
  `5c9b4ad6e4c56e3d3b5cc582baf9f7966e1bedd4`, built 2026-09-21.
- Plan source SHA256 remains
  `f702761ba5141608f311f1832db1f2ef0abbec190ea1e088fe4f857731e1f9a1`.

A new detached runtime worktree at the source HEAD has its own index, graph and
socket. No build ran there and no CARGO_TARGET_DIR was shared between worktrees.
All 100 daemon responses carry that HEAD and `dirty_count: 0`. The source
checkout itself retains the expected untracked experiment files.

### Serving-process verification and retained infrastructure failures

Index setup automatically started the new binary's daemon. The wrapper then
attempted a redundant foreground start: it exited 1 because the lock was already
held. Its provisional `daemon-identity.json` identifies that failed launch, **not
the serving process**, and its readiness duration is invalid as a startup metric.

Before teardown, `lsof` independently established that **PID 87820** owned the
exact replay socket and had the new `pixel-current` executable mapped; `ps`
confirmed its argv and isolated worktree. These corrected identity receipts are
`actual-daemon-{process,socket,executable}.txt`. The executable was not replaced
between launch and inspection. Thus the successful replay did use current-source
code, not the installed release. The serving experiment daemon was shut down
through its own socket; status confirmed it stopped. The original installed
0.4.0 daemon remained running and untouched. Daemon startup is **not measured**.

The first evaluation attempt failed before inference with missing `joblib`:
resolving the venv Python symlink selected the global interpreter. The second
wrapper preserves the venv path (`absolute()` rather than `resolve()`). No
candidate/scoring code changed, and no rows or predictions existed in the failed
attempt. Failure logs and both wrappers are retained.

## Frozen comparison

Same 60 training rows used only for baseline parity and same 40 diagnostic test
rows, labels, 20 families, scoring code and serialized learned model as the
[original pilot](plan-routing-pilot-results.md). No fitting, tuning or new variant.
A pre-run manifest now hashes the original gold report and gold-limitations
document as well as corpus, evaluator, model and build protocol. This binds the
new replay; it cannot retroactively repair the first run's provenance weakness.

| Measure | Installed release baseline | Source-built baseline | Frozen linear model |
|---|---:|---:|---:|
| Diagnostic exact sets | 12/40 | **12/40** | 24/40 |
| Macro-F1 | 0.423208 | **0.423208** | 0.744322 |
| Cost/task, FP + 2 FN | 2.175 | **2.175** | 1.000 |
| Novel-combination exact sets | 1/8 | **1/8** | 1/8 |

**100/100 query sets identical** between the two runtime baselines; all 40
candidate probability vectors are also exactly identical. Exact-set errors,
per-label confusion counts, bootstrap interval and calibration diagnostics are
unchanged. Raw findings need not be identical across index/worktree contents;
the measured observable is the selected query-name set only.

The corpus remains a small agent-authored synthetic challenge with overlapping
training/test themes. The conservative eight-row/four-family compositional slice
is too small for adoption and ties on the primary KPI. Four rows require concept
retrieval alongside a specialized query, which neither frozen output policy can
express. No repository holdout, real-traffic prevalence or downstream coding
success is established by replaying on a newer binary.

## Resource observations, not validated performance

The second host sample was only **56.20% idle**, with about **158 MB unused RAM**.
No dedicated latency repetitions were run. Build finished before evaluation.
Quality replay completed in **14.37 s** with `/usr/bin/time -l` process peak RSS
**122,748,928 bytes** (~117 MiB), excluding the separate daemon.

Candidate serialized-model/runtime load: **0.815 s**; first request **5.326 ms**;
remaining 39 requests median **1.290 ms**, p95 **1.613 ms**. Actual daemon Plan
median **92.412 ms**, p95 **143.315 ms** includes graph/history/concept retrieval;
it is not routing-only latency and is not comparable to candidate prediction
as an end-to-end speedup. All these clocks are unvalidated observations on a busy
host. Their difference from the first run is not a speed regression finding.

## Reproduction and evidence

Raw root: `.pixel/experiments/plan-routing-current-source-v1/`:

- `fetch.json`, `diff-origin-main.json`, `build.jsonl`, `build.stderr`,
  `build-identity.json`, `version.txt`, `pixel-current`;
- `protocol.json`, `pre-run-manifest.json`, isolated-worktree/index receipts;
- `attempt02/command.json`, `attempt02/eval-01/{freeze.json,raw.jsonl,summary.json}`;
- `comparison.json`, corrected `actual-daemon-*` identity/teardown receipts;
- both failed-launch logs, initial interpreter failure, host sample and gate logs.

The manifest includes full input/binary/model hashes. `comparison.json` compares
IDs, query sets, candidate probability vectors and all quality metrics against
the original raw run. Full inputs/model and original frozen evaluator remain in
the previous run's archive. The source replay archive is
`~/code/pixel-experiment-artifacts/plan-routing-current-source-v1.tar.gz`.

For a new reproduction, use fresh directories and the frozen source commit:

```sh
CARGO_BUILD_JOBS=4 cargo build --release --locked -p pixel-cli --message-format=json
# Create a NEW runtime worktree at the recorded commit; do not share a target dir.
git worktree add --detach /new/runtime/repo 5c894b1b3a5db713615d906dc8604805c73813ae
# Capture binary hash/version. Disable auto-start during index setup.
PIXEL_DAEMON_AUTO_START=0 target/release/pixel build-index /new/runtime/repo
# Launch this exact binary for the isolated repo and verify its socket owner.
target/release/pixel daemon start /new/runtime/repo
# Use the venv Python path without resolving its interpreter symlink.
.pixel/experiments/plan-routing-pilot-v1/venv/bin/python scripts/bench/bench-plan-routing.py evaluate \
  --model .pixel/experiments/plan-routing-pilot-v1/fit \
  --test scripts/fixtures/plan-routing-test.jsonl \
  --output /new/results/eval --socket /socket/reported/for/new/runtime/repo
# Stop only the experiment daemon after retaining the raw responses.
```

Executed checks: successful default-feature source build; **8 prototype tests**;
`scripts/ci/gates.sh` **67 tests (36+12+9+10), exit 0**, explicitly skipping Cargo
fmt/test/clippy for docs/bench-only changes; 100-row source/release baseline
comparison and 40-vector learned-model parity. No Rust edits, full workspace
tests, mutants, install/doctor, push, PR or publication. No shared target dir.

## Final next-step decision

1. **Keep current-source routing as the production default.** Do not integrate
   the learned prototype or spend on another model based on this diagnostic.
2. **Define mixed-query semantics before changing the router.** Decide whether
   concept lookup should coexist with specialized scans, and how a mixed request
   is mapped to each query's input. The pilot identifies a gap but does not
   implement or validate its fix.
3. **Collect independently reviewed real coding requests**, with useful-query
   evidence and task-family separation established before development. Freeze
   an untouched test partition. Then compare existing rules, a minimal
   deterministic candidate and the sparse linear approach under the agreed
   contract. Measure latency only if useful quality improves.

File ranking remains the longer-term product priority; it still needs judged
candidate relevance, including relevant unchanged files and genuine negatives.
This replay supplies no reason to conclude that unreleased ranking fixes are
ineffective: ranking was not the observable being tested.
