# Brief relevance beyond 90%, 2026-10-19

The final matched public benchmark source review is 45/56
(80.4%) for baseline and 53/56
(94.6%) for candidate, with
8 gains and 0 losses against that baseline. These are source-
relevance judgments, separate from native hook completion and probe execution.

Native run status is separate from source relevance. The final baseline status is
**failed: 49 accepted responses, 7 partial**; candidate status is **failed: 53 accepted responses, 3 partial**. The baseline has 7 partial native responses; the candidate has 3. Per-question source judgments and native reasons are preserved in the ledgers and run reports. A useful
saved partial brief remains a partial native delivery and does not turn a timed-out
probe into a successful run. Latency is reported as observed, not as a statistical
performance claim.

Inputs and frozen homes were matched, but host load was uncontrolled. Recorded load
windows were 49→92 for the r3 baseline, 9.17→55.78 for the final candidate, and
40.63→24.93 for the supplemental run. These are confounded observations; they do not
establish a causal speed change, and an individual timeout cannot be attributed to
host load from these measurements. Final runtime can differ.

An overall accepted native response does not mean every probe succeeded. Probe-level
failures and timeouts are reported separately for all four arms below; `empty` means
an ordinary abstention.

The final candidate returned nonempty briefs for all 56 public questions; the
supplemental run returned nonempty briefs for all 18 project-positive
questions, while all 70 controls returned zero bytes. These
delivery counts do not imply relevance. The first supplemental development set scored
9/18 (50.0%) on its
positive questions. It then informed implementation changes, so it is not held-out
evidence. The later supplemental run has 15/18 relevant among positive questions; 18/18 reviewed; 0 pending. Its native status is
**failed: 17 accepted responses, 1 partial, 70 abstained**. The fixed set contains 70
continuation and unrelated controls; 70 emitted zero brief bytes. This is
not a production-readiness or general zero-call claim. An intermediate `bf180014` candidate scored 50/56 on source relevance. This was a real implementation regression: CI #1262 rejected Rust-incompatible lookaheads in the flow-rank manifest, so the loader omitted that probe metadata. The diagnostic run and its failure remain archived; later measurements use the corrected manifest.

Recount judgments, verify identities and check the complete archived evidence:

```sh
rtk bypass python3 docs/bench/brief-relevance-2026-10-19-over90/verify.py
```

| Final matched native-hook measurement | Baseline | Candidate |
| --- | ---: | ---: |
| Source-reviewed relevant briefs (reviewed; pending) | 45/56 (56; 0) | 53/56 (56; 0) |
| Native run status | failed: 49 accepted responses, 7 partial | failed: 53 accepted responses, 3 partial |
| p95 observed elapsed time | 3029 ms | 3008 ms |
| Maximum observed elapsed time | 3039 ms | 3766 ms |

Earlier diagnostic runs recorded 45/56 relevant at the
baseline snapshot and 53/56 at the first
candidate snapshot (complete: 56 accepted responses; complete: 56 accepted responses).
They are chronology, not the final matched comparison. The repeat intentionally covers
only changed public-question briefs. Of 16
candidate briefs whose hashes differ from baseline, 16 were repeated
and 0 were not; it contains 0 IDs outside that
changed set. The repeat source review is 16/16 relevant,
16 reviewed, 0 pending. The repeat status is
**failed: 15 accepted responses, 1 partial**. Among 16
IDs shared by candidate and repeat, 14 are byte-identical and 2
differ. This is a subset repeat, not a full56 rerun. Repetition does not repair a
partial or failed native run.

| Run | Question | Probe | Outcome | Probe time |
| --- | --- | --- | --- | ---: |
| baseline | `pixel-03` | `config-registry` | `timeout` | 1258 ms |
| baseline | `pixel-04` | `error-origin` | `timeout` | 1602 ms |
| baseline | `pixel-07` | `error-origin` | `timeout` | 2008 ms |
| baseline | `ripgrep-07` | `error-origin` | `timeout` | 2009 ms |
| baseline | `requests-02` | `config-registry` | `timeout` | 2016 ms |
| baseline | `requests-02` | `doc-sections` | `timeout` | 1267 ms |
| baseline | `requests-02` | `flow-rank` | `timeout` | 1504 ms |
| baseline | `excalidraw-02` | `config-registry` | `timeout` | 1223 ms |
| baseline | `excalidraw-03` | `config-registry` | `timeout` | 1858 ms |
| baseline | `excalidraw-04` | `cand-j-chain` | `timeout` | 2287 ms |
| baseline | `excalidraw-07` | `error-origin` | `timeout` | 1891 ms |
| baseline | `excalidraw-07` | `name-files` | `timeout` | 1532 ms |
| baseline | `flask-04` | `cand-j-chain` | `timeout` | 2510 ms |
| baseline | `flask-04` | `flow-rank` | `timeout` | 1512 ms |
| baseline | `flask-07` | `error-origin` | `timeout` | 2045 ms |
| baseline | `fd-05` | `history-fallback` | `timeout` | 1514 ms |
| candidate | `pixel-03` | `config-registry` | `timeout` | 2008 ms |
| candidate | `pixel-04` | `cand-j-chain` | `timeout` | 1169 ms |
| candidate | `pixel-04` | `error-origin` | `timeout` | 1169 ms |
| candidate | `pixel-07` | `error-origin` | `timeout` | 1790 ms |
| candidate | `ripgrep-08` | `error-origin` | `timeout` | 2006 ms |
| candidate | `excalidraw-07` | `error-origin` | `timeout` | 2005 ms |
| candidate | `excalidraw-08` | `cand-j-callers` | `timeout` | 2312 ms |
| candidate | `express-07` | `error-origin` | `timeout` | 2007 ms |
| supplemental | `para-pixel-04` | `error-origin` | `timeout` | 2006 ms |
| supplemental | `para-excalidraw-08` | `cand-j-callers` | `timeout` | 2062 ms |
| supplemental | `para-excalidraw-08` | `doc-sections` | `timeout` | 1209 ms |
| repeat | `pixel-04` | `error-origin` | `timeout` | 1809 ms |

Exact oracle-line overlap is retained in scorer reports; it is separate from source
relevance and does not determine the manual judgment. Intermediate deadline failures
remain archived, including failures during severe host contention. No native deadline
was enlarged to improve a score.

## Fixed relevance criterion

A brief passes when it contains an exact substantive excerpt that answers a requested
fact or correctly locates the requested source: an architecture anchor, configuration
behavior, exact test, caller, error-producing path, flow boundary, or requested commit.
Lexical neighbors, wrong features and uncertain cases fail. Partial useful evidence can
pass source review; nonempty output alone cannot. Source review does not erase native
timeouts or partial status. Reviews are assistant source reviews, not blinded human
evaluation, and passing does not mean every excerpt is useful or that an agent can
finish without tools.

## Recovered public questions

| Question | Source evidence in the native brief |
| --- | --- |
| `pixel-03` | crates/pixel/src/execution_brief/chain.rs:53-58 |
| `ripgrep-03` | crates/core/flags/hiargs.rs:178-185; crates/core/flags/defs.rs:7088-7120 |
| `excalidraw-02` | packages/common/src/constants.ts:367 defines MAX_ZOOM=30; brief also shows viewport clamp using minZoom/maxZoom. |
| `excalidraw-04` | packages/excalidraw/actions/actionHistory.tsx:69-80; packages/excalidraw/actions/manager.tsx:97-150; packages/excalidraw/history.ts:139-147 |
| `flask-08` | src/flask/ctx.py:416-441 shows AppContext.push and request matching/session behavior; ctx.py:506-509 shows __enter__ calling push; app.py:1596 shows wsgi_app calling ctx.push. |
| `fd-06` | tests/tests.rs:1973-2012 |
| `express-04` | lib/express.js:36-39 shows the application function forwarding req/res/next to app.handle; lib/application.js:159-181 shows app.handle preparing req/res then calling this.router.handle(req,res,done). |
| `express-08` | lib/request.js:63-83 defines req.get/header; lines214-218, 297-315, 418-431, and 508-511 show req.range, protocol, host, and xhr callers. |

## Remaining public misses and rationale

| Question | Why it remains unresolved |
| --- | --- |
| `requests-07` | Covers retries and ChunkedEncodingError candidates, not max-retries error text origin. |
| `excalidraw-08` | The brief shows selection-ID handling and unrelated WOFF2 helpers; it does not locate the fresh element-ID generator or modules that call it. |
| `express-07` | The response module excerpt is generic and the 404 example is an app-defined page, not Express’s default “Cannot GET” formatter. The exact formatter is external/unavailable, so the emitted brief does not establish its source. |



Requests imports and catches urllib3's `MaxRetryError`; its declared range is
`urllib3>=1.26,<3`. Express imports `finalhandler` with range `^2.1.0` and passes its
fallback callback to routing. The exact message formatters are absent from the pinned
project checkouts. Project source can prove the local handoff, but cannot identify the
exact implementation of a range-selected dependency. Both exact-origin questions
Requests-07 and Express-07 remain failures under the fixed relevance criterion. A version-resolved
dependency-source index would be needed to inspect those formatters; fetching an
arbitrary current dependency would not prove the pinned project's runtime behavior.
No runtime network lookup or LLM dependency was added.

## Supplemental questions and controls

The supplemental set contains 11 rewordings of original misses, seven fresh questions
(one per project), and ten controls repeated across seven projects. Its SHA-256 is
`ac1618f591d8010835f4c911176346a3cdf1ce60a6ae011b3b67fefc8a4620be`. The later results
are development-set measurements, not held-out evidence. Negative prompts include
continue, ok, yes, acknowledgments, weather, and unrelated general-knowledge or
creative prompts.

Remaining supplemental misses:

| Question | Rationale |
| --- | --- |
| `para-excalidraw-08` | The brief follows getSelectionStateForElements and related selection modules; it does not locate the fresh element-ID factory or callers. The adjacent canvas/selection evidence does not answer the generator-caller request. |
| `para-requests-07` | The exact Max retries exceeded constructor/text is outside the pinned Requests source in urllib3. Local wrapper evidence is insufficient under this exact-origin prompt; no in-scope local recovery can prove the formatter. |
| `para-express-07` | The brief contains response helpers and an application-specific download 404 example, but does not locate the standard no-route GET not-found generation. The default formatter is external/unavailable here; these adjacent snippets do not establish its origin. |



The first supplemental scorer exited 1 because 42 empty controls were labeled
malformed when the Pi hook response `{"decision":"observe"}` omitted `context`.
All saved brief outputs were already empty. The scorer fix recognizes that specific
observe-without-context shape as abstention while retaining other validation rules.
The original rejected report is retained, not rewritten.

## Frozen inputs and verification

- Native binary source: `1d1c063de058fb132d84623e67a199aaf3308904`.
- Binary SHA-256: `0772041bc814c6bfd65c76c5a70c29fe60537dd94649fec143cc48849385a38c`.
- Baseline probe source: `412d302f42157532b10092f81c4c2ba8f52b7e63`.
- Final candidate probe source: `df9f39fa9696432e0bdb73bc77fc847a2dd96513`.
- Both final arms use the same binary, question file, scorer, model and configuration.
  Candidate probes are overlaid in an isolated home; no Rust build is substituted.
- Separate roots were copied from one prepared seed and verified equal before use.
  Root receipts, command lines, source pins and probe hashes are archived.
- The optional intent LLM classifier is disabled in both homes. The local
  embedding/ranking model and configuration are matched; retrieval additions use
  local source only.
- Repeated outputs are compared by ID and brief SHA-256 above. Earlier relevance
  judgments are reused only for byte-identical output at the same source commit.

Eight PRs have archived current-head CI receipts. Verify every PR head and required
check before treating the candidate as ready; a receipt is not a merge authorization.
Focused unit suites cover config selectors, exact option tests, imported factory
bindings, same-receiver methods/getters, source masking, caller identity, flow
handoffs, and admission controls. Named-import recovery is supported; default imports,
namespace imports and CommonJS `require` remain unsupported. No mutation campaign,
managed installation, release or merge was performed.

| Pull request | CI-tested head |
| --- | --- |
| [#1255](https://github.com/Pixel-CLI/pixel/pull/1255) | `8ca13a7b483d7e908d80a58dfcd5e2c000d77841` |
| [#1256](https://github.com/Pixel-CLI/pixel/pull/1256) | `72bd82d7e6f8584f36e4b0caf133c6a23a6ed50e` |
| [#1257](https://github.com/Pixel-CLI/pixel/pull/1257) | `71f157ed014e30bb72fd10689dfcd80e87cc79c1` |
| [#1258](https://github.com/Pixel-CLI/pixel/pull/1258) | `745d81f2588ca081bc0a4e8664c7a270df69a901` |
| [#1259](https://github.com/Pixel-CLI/pixel/pull/1259) | `866aa962e53a8757b512f17849cab9464e39cb59` |
| [#1262](https://github.com/Pixel-CLI/pixel/pull/1262) | `010c46bdd97007497dec8e6aaa630becd9ddcbd8` |
| [#1263](https://github.com/Pixel-CLI/pixel/pull/1263) | `eb295c602e18ad231bcbc89b7f5ae836e9be3eaf` |
| [#1265](https://github.com/Pixel-CLI/pixel/pull/1265) | `a6fd332114e960d9e84e106ac6e6fab764f04d42` |

The broader [#906](https://github.com/Pixel-CLI/pixel/issues/906) equal-answer and
zero-tool-call objective remains open. These relevance counts do not satisfy it.

## Artifacts

The archive contains native reports, source-review ledgers, compressed full briefs,
repeat outputs, identity receipts, CI receipts, runner scripts and intermediate
diagnostics. The final direct flow receipt embeds full stdout/stderr and is also
expanded into per-case text artifacts; the Excalidraw profile receipt is preserved
beside it. `repeat-selection.json` records 16 selected changed public cases and
the archived `repeat_changed.py` reproduces that selection. Use `artifact-hashes.json`
and `verify.py` to verify archived files.
The prior report remains unchanged in `../brief-relevance-2026-10-19/`.
