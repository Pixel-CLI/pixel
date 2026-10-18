# Error-origin evidence qualification — #1231, 2026-10-19

Historical pre-review result. See the [critical review and corrected candidate](review/README.md)
for the current conclusion, restored ripgrep-07 context and residual flask-07 loss.

Verdict at this snapshot: improved evidence, with a measured coverage gain and one regression.
PR #1241 addresses #1231; #1197 remains closed. The rejected audit prototype was not ported.

## Hypothesis and controls

Hypothesis: distinguish syntax-backed evidence roles from literal/name matches,
quote a matching Python conversion/rethrow handler and import boundary, and
leave lexical call/test links unresolved without extending them into causal chains.
A scratch prototype passed seven controls before integration: docstring-only,
local emission, message definition, conversion, rethrow, homonymous receiver and
requests-07. No benchmark paths, error types or answer strings select a code path.
The previous caller-by-name prototype was rejected in the [#1197 audit](https://github.com/Pixel-CLI/pixel/issues/1197#issuecomment-6255142588).

`python3 -I probes/error-origin/tests/test_probe.py`: 11 tests pass. The five new
regression methods (`python3 -I /tmp/pixel-1231/regression_before.py`) run against main `7b178f5a`'s unchanged probe produce seven
failing assertions and zero harness errors. `python3 -I scripts/bench/probe-test.py
--self-test`: 24 pass; `--check-all`: ok. SPDX, Python compilation and whitespace
checks pass. The source excerpt now retains its last line when below the cap.
Only output-contract assertions change in probe-test; no scorer code changes.

The shipped offline probe judge, using the same fixed native base briefs instead
of the unfrozen command route, reports OK over all 56 questions: seven runs,
deterministic x2, read-only, no timeout or stray output. This is diagnostic replay,
not a tool-call or answer-quality claim. The lexical overfitting guard flags four
prompts (`parse`, `raises`, `runtime`, `cannot`): manual source review identifies
standard `ast.parse`, the raise-node list and epistemic prose, not benchmark answers.
All raw controls and judge outputs remain under `/tmp/pixel-1231/`.

## Frozen native comparison

Protocol v4 from [the shared reference](../scorer-protocol-v4-2026-10-19/README.md).
Measured candidate `b634bd8cc68ef88407f2683d11ed8f0974848643`, based on fresh main `7b178f5a`.
Binary source `a0d99d62`, SHA-256 `fcde2a41d4d0e7017acdba2ee58e4803afa9379b8b3995b68e24b71ec14d8e64`.
Scorer SHA-256 `95bf349407a86091b7627f0d7726697478408b743345b32d4ed919da36233106`; questions SHA-256 `984a632e96f5f32891279d58f872c3a891bfeabaee110b3993be313c63e17069`.
Same seven pins, 14 probes, warm state, hook Pi brief_only, three parts / 3000 ms,
300 s process timeout, dedupe disabled. Only error-origin program/manifest differ.
Both complete/exit 0: 53 ok + 3 abstained, no errors, partials or cuts.
Abstentions stay in the denominator. Benchmark and judge serialized using
`/tmp/pixel-benchmark.lock`; the lock is cooperative, not a guarantee of an idle machine.

Exact argv, all probe hashes, pins and raw-report hashes: [identity.json](identity.json).
Reproduce from a checkout whose scorer has the hash above (later docs/test-only
commits retain the measured probe bytes). Use a fresh label/output directory
when repeating a run to preserve the recorded evidence:

```sh
python3 -c 'import fcntl,subprocess,sys; f=open("/tmp/pixel-benchmark.lock","a"); fcntl.flock(f,fcntl.LOCK_EX); sys.exit(subprocess.call(sys.argv[1:]))' \
  python3 -I scripts/bench/tool-census/score_brief.py pinned \
  --base /tmp/pixel-1197-audit/pixel \
  --probe-home /tmp/pixel-1197-audit/runs/probe-home-a0d99d6268b7 \
  --head-probe-commit b634bd8cc68ef88407f2683d11ed8f0974848643 \
  --repo /Users/navid/code/personal/pixel-1231-error-evidence \
  --questions /Users/navid/code/personal/pixel-1231-error-evidence/eval/brief-bench/questions.jsonl \
  --route hook --root /tmp/pixel-1231/state --out /tmp/pixel-1231/runs --label evidence1231
```

| Category | n | file_recall base → head | region_cov base → head |
|---|---:|---:|---:|
| all | 56 | 0.425378 → 0.425378 | 0.084869 → 0.087198 |
| config | 7 | 0.370308 → 0.370308 | 0.085233 → 0.085233 |
| diagnosis | 1 | 0.263158 → 0.263158 | 0.120877 → 0.120877 |
| error-origin | 7 | 0.254737 → 0.254737 | 0.050879 → 0.069507 |
| flow | 7 | 0.366213 → 0.366213 | 0.147829 → 0.147829 |
| history | 7 | 0.791444 → 0.791444 | 0.119940 → 0.119940 |
| impact | 6 | 0.183952 → 0.183952 | 0.000671 → 0.000671 |
| lookup | 7 | 0.297357 → 0.297357 | 0.063135 → 0.063135 |
| overview | 7 | 0.796465 → 0.796465 | 0.155627 → 0.155627 |
| tests | 7 | 0.331233 → 0.331233 | 0.038466 → 0.038466 |

Exact metrics/deltas: [56 questions](per-question.jsonl), [categories](per-category.jsonl).
54/56 questions keep their coverage metrics. 50/56 briefs are byte-identical;
excalidraw-07, flask-04, flask-07 and fd-07 change text/qualification without
changing coverage. The two metric changes are:

| Question | file_recall | region_cov base → head | line_cov base → head |
|---|---:|---:|---:|
| ripgrep-07 | 0.105263 → 0.105263 | 0.003872 → 0.003125 | 0.100588 → 0.081176 |
| requests-07 | 0.750000 → 0.750000 | 0.005658 → 0.136806 | 0.047009 → 0.205128 |

requests-07 gains the actual caught-type import and `HTTPAdapter.send` handler,
including `conn.urlopen`, the reason-specific branches and the fallback
`ConnectionError(e)`. These establish local conversion syntax. The formatting
of the text in urllib3 is outside the pinned repository and remains unverified.
ripgrep-07 loses second-level lexical context: the policy now stops at one
unresolved level instead of suggesting a causal chain. This is a coverage
regression, not a demonstrated answer-quality improvement for that question.

## Limits and propagation

Producer: error-origin stdout. Consumers: the unchanged bundled-probe registry
and native host/merge; diagnostic probe-test and v4 scorer parse the same
headers/numbered-line contract. The manifest version is 0.3.0. No new stored
field, graph/extractor version, scorer protocol, command or install target.

Python syntax roles are bounded heuristics, not runtime tracing. Other languages
retain candidate matches. Imports may be shadowed; method/receiver bindings are
not resolved. Source scans and snippets are capped as documented in the
[probe contract](../../../probes/error-origin/README.md). No real agent campaign,
quality-reviewed zero-call result, local installation, Rust or mutation campaign.
Model caches, PATH tools and OS remain outside the frozen protocol. CI must
validate the final PR head; no merge is authorized by these results.
