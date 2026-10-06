# Deterministic prompt fact-pack experiment harness

Evaluates whether a bounded deterministic repository-fact packet reduces
total agent trajectory cost while preserving verified correctness. The protocol
is `docs/bench/prompt-fact-pack-experiment.md`; this directory is the harness
that runs it.

## Arms

| arm | retrieval setup |
| --- | --- |
| `no-pixel` | No Pixel prompt or hook. |
| `current-pixel` | Current installed Pixel guidance/hook behavior. |
| `fact-auto` | Every substantive prompt receives the at-most-1 KiB packet when fresh facts are available. |
| `fact-ondemand` | The harness requests the canonical full JSON fact result only when it chooses to. |

The automatic packet comes from `pixel scope-task --read-only`: a pure read
of a compatible warm daemon's already-published index and graph. It never
starts a daemon, builds or refreshes an index, or writes a manifest. An
unavailable, stale, malformed or timed-out response contributes no packet.
Pixel is a fact oracle in every arm; lifecycle routing is out of scope.

## Run

Offline (default) — no provider request, no network:

```bash
python3 eval/fact-pack/run.py \
  --families eval/fact-pack/scenarios \
  --out results/fact-pack-offline \
  --pixel "$(command -v pixel)"
```

The offline mode uses a deterministic local fake model, so the whole pipeline
(runs, frozen-input and trajectory recording, decision rule) is exercised
without a provider. It verifies the harness, not a measurement.

Live — unavailable: provider dispatch is not implemented. Passing `--live`
raises `RuntimeError`. Only the offline mode is functional.

## What it writes

```
results/fact-pack-offline/
  frozen-inputs.jsonl   one record per fact request: the frozen inputs that
                        identify a fact result, the repo signature, and the
                        injected packet size
  trajectories.jsonl    one record per run: model/harness versions, arm,
                        task-family and pair ids, verified success/failure/
                        timeout, elapsed, API/token usage, Pixel/tool calls,
                        files inspected, test time, edits, rework
  verdict.json          the decision-rule verdict for each candidate arm
```

## Decision rule

A candidate arm (`fact-auto` or `fact-ondemand`) must be non-inferior to both
baselines on verified success — the one-sided 95% confidence bound of the
paired difference in verified-success proportions must be greater than -5
percentage points — and show at least a 10% paired task-family improvement
in verified completion time, with its interval reported. The verdict also
reports failures, timeouts, total cost per verified completion,
injected-packet tokens, unavailable-rate and misleading candidates.

## Tests

`scripts/test-fact-pack-harness.py` drives the whole pipeline offline against
a disposable repository and a fake pixel, and checks the decision rule on
hand-written trajectories. No model is called.
