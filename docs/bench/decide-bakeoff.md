# decide-bakeoff — small open decision models on Pixel's surfaces

Measured 2026-09-22 on macOS arm64, CPU only, per the frozen protocol in
`decide-bakeoff-protocol.md`. Harness: `scripts/bench/bench-decide/` (run1, all 8
adapters, 239 records each, zero errors). Raw rows and manifest are under
`scripts/bench/bench-decide/runs/run1/`.

## Headline answer

**No existing ≤500M local model beats Jev on coding decisions.** Jev's
published coding-topic accuracy is 0.839 (n=56, all tiers); the best measured
candidates here reached 0.500 on the 14-item public coding subset. The
"beat Jev with a 100M model" hypothesis is not true for off-the-shelf
checkpoints — the gap is coding-specific training data, not parameters.

**On Pixel's own plan-routing surface, the shipped keyword router already
beats every learned model by a wide margin** — and the static embedding
backend beats every small decision model except the 575M/8 GB GLiFormer.

## Results — coding-public (14 public JevBench coding items)

| Model | Params | Accuracy | F1 | ECE | p50 | p95 | Load | Peak RSS |
|---|---|---|---|---|---|---|---|---|
| verdict | 151M | **0.500** | 0.212 | 0.301 | 58 ms | 131 ms | 0.6 s | 1.5 GB |
| gliformer | ~575M | **0.500** | 0.146 | 0.500 | 143 ms | 333 ms¹ | 6.6 s | 8.2 GB² |
| laya | 421M | 0.429 | 0.137 | 0.374 | 40 ms | 212 ms | 20.6 s | 4.0 GB |
| openjev-deberta | 435M | 0.429 | 0.150 | 0.438 | 592 ms | 1459 ms | 0.7 s | ² |
| pixel-static | 128M | 0.429 | 0.137 | 0.231 | 0.5 ms | 0.8 ms | 0.001 s | 0.03 GB |
| gavel | 150M | 0.357 | 0.135 | 0.334 | 62 ms | 134 ms | 2.9 s | 1.1 GB |
| gte-reranker | 149M | 0.286 | 0.067 | 0.326 | 59 ms | 123 ms | 1.0 s | ² |

¹ Per-set p95; gliformer's plan-routing p95 was 7.1 s.
² Peak RSS is the runner process's cumulative peak (`peak_rss_bytes` in
run1's manifest, `getrusage(RUSAGE_SELF)` after each model): every model ran
in one process, in the order pixel-static, gavel, verdict, laya, gliformer,
openjev-deberta, gte-reranker, so a figure can include memory still held from
an earlier model and is an upper bound for that model. openjev-deberta and
gte-reranker ran after gliformer, so their peaks are not separable — both are
PyTorch transformers and land in the 1–2 GB class. Child processes are not
counted: pixel-static answered through a `pixel classify` child, and its
0.03 GB is the Python runner's own footprint, not the model's.

Published coding-topic rows (n=56, **not reproduced**, different
denominator): Jev 0.839, SemIf-4B 0.964, gliformer 0.679, laya 0.589,
verdict ~0.52, openjev-deberta 0.464.

## Results — plan-routing (45 items × 5 labels = 225 noul decisions)

| Model | Accuracy | Exact-set (45) | F1 | ECE | p50 | p95 |
|---|---|---|---|---|---|---|
| **keyword-router** (shipped) | **0.813** | **0.578** | 0.718 | 0.187 | 0.4 ms | 1.4 ms |
| gliformer | 0.644 | — | 0.392 | 0.378 | 1203 ms | 7108 ms |
| pixel-static | 0.547 | — | 0.394 | 0.039 | 0.5 ms | 1.7 ms |
| laya | 0.267 | — | 0.258 | 0.434 | 74 ms | 124 ms |
| openjev-deberta | 0.244 | — | 0.236 | 0.402 | 366 ms | 1535 ms |
| gavel | 0.236 | — | 0.231 | 0.433 | 106 ms | 173 ms |
| verdict | 0.200 | — | 0.167 | 0.442 | 130 ms | 151 ms |
| gte-reranker | 0.200 | — | 0.167 | 0.501 | 152 ms | 256 ms |

"—" marks an exact-set of 0/45 in `runs/run1/summary.json`. Every gold set
in this frozen index is exactly `{by-concept}` (no positive for any
specialized query, no mixed-query item), so exact-set here counts only items
with no specialized false positive: a constant `{by-concept}` answer would
score 45/45. The keyword router's 0.578 is 26 items where none of its
keyword rules fired, not evidence of specialized recall; the protocol's
coverage precondition for the integration gate is not met.

Every learned model collapses toward "yes" on routing nouls — verdict said
yes to all 225 propositions. The task ("does this issue call for dead-code
analysis?") is a domain-specific multi-label decision the generic models
over-fire on; the keyword router's name-set rules capture it almost free.

## What shipped — remote-only `pixel classify`

`pixel classify` maps the same wire contract (`text` +
`context` + `labels` + `criteria` → probability distribution) onto an
OpenAI-compatible chat completion. Remote is the **only** engine — the
local `static` (Model2Vec cosine) and `verdict` (ONNX) backends and the
`--backend` flag were removed after this bake-off showed no off-the-shelf
local model beats Jev. One adapter (`crates/pixel/src/decide_remote.rs`)
serves all three presets via `--remote-preset`:

| Preset | Base URL | Key env | Default model |
|---|---|---|---|
| `openrouter` | `openrouter.ai/api/v1` | `OPENROUTER_API_KEY` | `deepseek/deepseek-v4.1-flash` |
| `ollama` | `ollama.com/v1` | `OLLAMA_API_KEY` | `deepseek-v4.1-flash:cloud` |
| `local` | `localhost:11434/v1` | — | `qwen3.5:4b` |

Env overrides on top of the preset: `PIXEL_REMOTE_MODEL`, `PIXEL_REMOTE_BASE`,
`PIXEL_REMOTE_KEY_ENV`. The local 4B path is the *same* adapter pointed at a
`llama-server`/Ollama on localhost (`scripts/dev/decide-local-setup.sh` downloads
the GGUF), so no new Rust inference code — a 4B model runs decisions on your
own CPU/GPU at $0 marginal cost.

- The spec's `context` becomes the system framing, `text` the state, and
  labels + criteria are enumerated verbatim (arbitrary runtime labels
  preserved).
- The POST asks for a strict `response_format` JSON schema; the reply's
  `probs` (nested or flat — the adapter accepts both, since local/Ollama
  models frequently ignore the schema and emit a flat map) is verified and
  renormalized to sum 1. Unknown labels, non-finite values, and zero-sum
  replies are errors, not silent renormalisations.
- Disclosure is explicit: `snapshot.deterministic=false`,
  `snapshot.provider` = preset, `snapshot.model` = model id, and
  `basis` = "remote LLM, non-deterministic, verbalized probabilities
  (self-reported, renormalized to sum 1)". The API key is read from the env
  var by name and never logged (the `Config` `Debug` impl masks it).
- HTTP/parse failures answer `{"ok":false,"error":…}` on that line;
  `--jsonl` keeps serving subsequent lines.

The rows below were measured while the flag still existed as
`--backend remote`; the command is now just `pixel classify`.

## Results — remote/cloud models on coding-public (14 items)

Measured 2026-09-23, `run.py --models remote-cli --sets coding`, one chat
completion per spec, verbalized probabilities. Run dirs:
`runs/remote-{coding,deepseek-v4,gptoss-120b,gptoss-20b,nemotron-ultra}`.

| Model | Provider (preset) | Score | vs Jev 0.839 | p50/item | TPS² |
|---|---|---|---|---|---|
| deepseek-v4.1-flash | Ollama Cloud (`ollama`) | **14/14 = 1.00** | ✅ beats | 1.4 s | 173 |
| deepseek-v4-flash:0731 | Ollama Cloud (`ollama`) | **13/14 = 0.93** | ✅ beats | 2.0 s | 77 |
| gpt-oss:120b | Ollama Cloud (`ollama`) | **13/14 = 0.93** | ✅ beats | 2.0 s | 176 |
| gpt-oss:20b | Ollama Cloud (`ollama`) | **13/14 = 0.93**⁴ | ✅ beats | 6.4 s | 99 |
| nemotron-3-ultra | Ollama Cloud (`ollama`) | **12/14 = 0.86** | ✅ beats | 8.1 s | 72 |
| google/gemini-3.1-flash-lite | OpenRouter (`openrouter`) | 11/14 = 0.79 | ❌ | 1.1 s | — |
| qwen3.5:397b | Ollama Cloud (`ollama`) | 10/14, 9/14 | ❌ | 16.2 s¹ | — |
| gemma4:31b | Ollama Cloud (`ollama`) | 0/14 | ❌ | — | — |
| **Jev** (reference) | TypeSafe AI service | **0.839**³ | — | — | — |

¹ Every item qwen answered was correct (19/19 across two runs); the losses
were a global timeout on the 33 s `long_policy` item and intermittent
non-JSON replies — harness limits, not wrong answers. gemma4:31b never
emitted valid JSON (prose only, confirmed via direct API call).
nemotron-3-ultra's two losses were timeouts; all 12 answered items were
correct.

² TPS from [ollamatps.com](https://ollamatps.com) (Ollama Cloud Pro tier,
latest measured run, fetched 2026-09-23).

³ Jev's published coding-topic accuracy, n=56, all tiers — published
number, not re-measured here; different denominator than our n=14 subset.

⁴ gpt-oss:20b's one loss was a harness failure, not a wrong answer: on
`hard-opus-c-long_policy-05` the reply was not JSON (`remote response content
is not JSON`, `runs/remote-gptoss-20b/remote-cli.raw.jsonl`); all 13 answered
items were correct.

OpenRouter note: `api.openrouter.ai` was NXDOMAIN during the run; the
preset now defaults to the working `openrouter.ai/api/v1` base. Keys come
from the env var named by the preset; keep them in your own secret store.

## Honesty notes (decision backends)

- **Remote rows are non-deterministic**: probabilities are *verbalized*
  (self-reported by the model), not native calibration-head outputs. ECE on
  a bake-off will expose this; treat remote confidence as evidence only of
  the model's self-assessment, not calibrated reliability.
- **JevBench numbers in the matrix are published, not re-measured here**;
  the 239-spec local bake-off (`runs/run1`) is the reproduced evidence for
  the deterministic local rows.
- OpenRouter's provider stats are their measured 30-day figures and exclude
  client RTT — a floor, not a guarantee. `qwen/qwen3.8-27b:free` exists as a
  rate-limited free tier.
- Ollama Cloud has no mid-size decision models (its library steps from
  `deepseek-v4.1-flash` / `gpt-oss:20b` up); it is an alternate provider for
  the same adapter, not a unique model line.

## Verdict per candidate

- **verdict — integrated, then removed.** Tied for best coding accuracy,
  60 ms class, 151M, Apache-2.0, published ONNX — the best small local
  *decision* model measured, but still below Jev, so the ONNX backend was
  removed when `pixel classify` went remote-only.
- **gliformer** — same coding accuracy, but 8 GB RSS and multi-second p95
  make it unusable on a hook path. Not integrated.
- **laya, openjev-deberta** — more parameters, worse results, no ONNX
  artifact. Ruled out.
- **gavel** — the wildcard missed: its published wins are API/tool
  selection, a different decision shape. Ruled out.
- **gte-reranker** — rerankers answer "is this doc relevant to this query",
  not bounded classification; measured to confirm, ruled out.
- **keyword-router** — confirmed: Pixel's shipped structural routing is
  already the best available for plan routing. No change needed.

## What would actually beat Jev on coding

The measured gap (0.50 vs 0.839) plus the router result point the same way:
the missing ingredient is coding-decision supervision, not capacity. The
distillation path from the original plan — Jev/strong-teacher labels over
Pixel-specific decisions → 50–150M encoder → ONNX — remains the plausible
route; it was excluded here only by the no-training constraint.

## Reproduce

```sh
# The ML adapters need a venv with torch, transformers and the model authors'
# packages (models.py); keyword-router, remote-cli and score.py need only python3.
scripts/bench/bench-decide/.venv/bin/python scripts/bench/bench-decide/run.py \
  --models keyword-router,gavel,verdict,laya,gliformer,openjev-deberta,gte-reranker \
  --out scripts/bench/bench-decide/runs/run2
python3 scripts/bench/bench-decide/score.py --run scripts/bench/bench-decide/runs/run2 --baseline keyword-router
```

`--out` must not exist yet. The `pixel-static` adapter left with the static
backend, so run1's baseline row cannot be regenerated; `score.py` can still
rescore a copy of `runs/run1` (it writes `summary.json` into the run
directory, so do not point it at the committed one). `freeze_sets.py` rewrites the committed
fixtures from a JevBench checkout (`--jevbench`), so run it only to re-freeze.

The shipped path was verified by piping the frozen fixtures through
`target/debug/pixel classify --jsonl` (remote; needs a provider key env).
