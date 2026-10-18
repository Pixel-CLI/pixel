# Public brief acceptance evaluation — 2026-10-17

**Verdict: 9/56 accepted; #906 remains open.** The maintainer chose the public
56-question set after repairing its five empty oracles. The frozen native binary
was unchanged during this evaluation. No retrieval improvement is claimed.

## Results

| Run | Questions | Brief calls | Fresh control calls | Brief quality failures | Control quality failures |
|---|---:|---:|---:|---:|---:|
| r1 | 56 | 78 | 235 | 20 | 3 |
| r2 | 17 | 2 | 55 | 1 | 1 |
| r3 | 17 | 2 | 60 | 1 | 2 |

The complete first pass used **78 versus 235 calls**. Its 34 zero-call brief
answers yielded 17 provisional candidates after reviewing both arms. Only these
17 pairs were repeated twice; repetition totals must not be pooled into a
56-question reduction estimate. Across all 180 sessions, the aggregate reports
record no protocol failures or late brief parts. Every brief received three parts.

Four candidates needed a tool on a later run: `requests-02`, `requests-07`,
`ripgrep-05`, and `excalidraw-02`. Four others failed quality in a later pair:
`flask-01` (invented exception exports), `flask-06` (missing test checks),
`fd-02` (control names nonexistent `Args::threads`), and `fd-08` (control wrongly
calls main the only process-exit boundary). The latter two remain unaccepted
even though their brief answers passed: the predefined protocol requires both
arms to pass. This is not evidence that those brief answers were wrong.

## Frozen inputs and method

- Pixel source: `ea0f78f444ff6d6d8fa2327e06548523edf00cd3` (dev-release).
- Native SHA-256: `ce1b238d3cd58e2cf1cefd3e8dedf64f1983afcc6618b3b45735d3da084b4a23`.
- Runtime harness: `1c2ed4304e68f4f63080df59f7c2821292c0498f`; the later offline
  acceptance verifier does not change live experiment inputs.
- Question SHA-256: `984a632e96f5f32891279d58f872c3a891bfeabaee110b3993be313c63e17069`.
- Codex 0.163.0, `gpt-5.6-terra`, medium effort; three hook parts; four session
  workers, two index workers, 600-second session timeout; history enabled.
- Each pair used fresh workspaces at the question’s pinned repository commit.
  The control had no prompt-submit Pixel hooks. Calls include shell invocations
  (including Pixel commands); they are not estimates of equivalent file reads.
- All 13 bundled probe programs and manifests were frozen in an isolated home.
  Their hashes and exact launcher inputs are in each `identity.json`. Ordinary
  installed-home pilots were excluded because two installed probes differed.
- The acceptance protocol was written before this campaign in
  [the dataset README](../../../eval/brief-bench/README.md#acceptance-protocol-defined-before-the-new-live-campaign).
  Each accepted case needs three complete, nonempty, quality-passing pairs and
  zero brief calls in every repetition. One failed case defeats full-set success.

Reviews in `reviews.jsonl` are **Codex source review, not blinded and not an
independent human review**. Each verdict is bound to both answer hashes and
the full source pin, with findings and pinned source references. All 180
hashes/call counts were checked against the parsed local final answers; source
ranges were checked against Git objects and bounded to the file at that pin.
The offline verifier checks identity/protocol; it cannot judge meaning.
Maintainers can challenge these qualitative judgments. Raw answers, streams,
commands and temporary authentication files are not published. Hashes do not
allow a reader to reconstruct an answer; the published findings are the
reviewable summary, not a substitute for independently re-running the model.

The oracle guides review rather than defining every valid answer. For example,
Flask’s Python-version change has both a constituent commit and a reachable
merge commit: naming the latter was accepted. A control answer naming the
constituent but the wrong PR number failed. For `ripgrep-02`, identifying
binary policy selection plus the NUL heuristic and overrides was sufficient;
the actual `Core::detect_binary` classifier is useful additional precision.
These decisions are recorded to make review boundaries explicit.

## Per-question verdicts

`B/C` means brief/control source-review quality. Calls list r1/r2/r3; `—`
means no repeat was selected. An unselected repeat is not a zero.

| Question | Brief calls r1/r2/r3 | Control calls r1/r2/r3 | Quality B/C r1/r2/r3 | Accepted |
|---|---|---|---|---|
| excalidraw-01 | 5 / — / — | 6 / — / — | pass/fail, —, — | no |
| excalidraw-02 | 0 / 0 / 1 | 3 / 2 / 4 | pass/pass, pass/pass, pass/pass | no |
| excalidraw-03 | 3 / — / — | 4 / — / — | pass/pass, —, — | no |
| excalidraw-04 | 12 / — / — | 10 / — / — | fail/pass, —, — | no |
| excalidraw-05 | 0 / — / — | 4 / — / — | fail/pass, —, — | no |
| excalidraw-06 | 0 / — / — | 5 / — / — | fail/pass, —, — | no |
| excalidraw-07 | 2 / — / — | 3 / — / — | pass/pass, —, — | no |
| excalidraw-08 | 2 / — / — | 7 / — / — | pass/pass, —, — | no |
| express-01 | 0 / — / — | 2 / — / — | fail/pass, —, — | no |
| express-02 | 2 / — / — | 2 / — / — | pass/pass, —, — | no |
| express-03 | 2 / — / — | 4 / — / — | pass/pass, —, — | no |
| express-04 | 6 / — / — | 5 / — / — | fail/fail, —, — | no |
| express-05 | 1 / — / — | 3 / — / — | pass/pass, —, — | no |
| express-06 | 0 / — / — | 4 / — / — | fail/pass, —, — | no |
| express-07 | 2 / — / — | 4 / — / — | pass/pass, —, — | no |
| express-08 | 4 / — / — | 4 / — / — | pass/pass, —, — | no |
| fd-01 | 0 / 0 / 0 | 6 / 4 / 3 | pass/pass, pass/pass, pass/pass | yes |
| fd-02 | 0 / 0 / 0 | 2 / 3 / 2 | pass/pass, pass/fail, pass/fail | no |
| fd-03 | 0 / — / — | 4 / — / — | fail/pass, —, — | no |
| fd-04 | 0 / — / — | 7 / — / — | fail/pass, —, — | no |
| fd-05 | 0 / — / — | 3 / — / — | fail/pass, —, — | no |
| fd-06 | 3 / — / — | 5 / — / — | pass/pass, —, — | no |
| fd-07 | 3 / — / — | 2 / — / — | pass/pass, —, — | no |
| fd-08 | 0 / 0 / 0 | 4 / 4 / 5 | pass/pass, pass/pass, pass/fail | no |
| flask-01 | 0 / 0 / 0 | 3 / 4 / 3 | pass/pass, fail/pass, pass/pass | no |
| flask-02 | 0 / 0 / 0 | 1 / 1 / 1 | pass/pass, pass/pass, pass/pass | yes |
| flask-03 | 0 / — / — | 3 / — / — | fail/pass, —, — | no |
| flask-04 | 3 / — / — | 5 / — / — | pass/pass, —, — | no |
| flask-05 | 0 / — / — | 2 / — / — | pass/fail, —, — | no |
| flask-06 | 0 / 0 / 0 | 4 / 4 / 4 | pass/pass, pass/pass, fail/pass | no |
| flask-07 | 4 / — / — | 4 / — / — | pass/pass, —, — | no |
| flask-08 | 5 / — / — | 5 / — / — | pass/pass, —, — | no |
| pixel-01 | 0 / 0 / 0 | 4 / 4 / 4 | pass/pass, pass/pass, pass/pass | yes |
| pixel-02 | 0 / 0 / 0 | 3 / 3 / 6 | pass/pass, pass/pass, pass/pass | yes |
| pixel-03 | 0 / 0 / 0 | 3 / 5 / 3 | pass/pass, pass/pass, pass/pass | yes |
| pixel-04 | 6 / — / — | 9 / — / — | pass/pass, —, — | no |
| pixel-05 | 0 / — / — | 3 / — / — | fail/pass, —, — | no |
| pixel-06 | 0 / — / — | 5 / — / — | fail/pass, —, — | no |
| pixel-07 | 0 / — / — | 5 / — / — | fail/pass, —, — | no |
| pixel-08 | 0 / — / — | 5 / — / — | fail/pass, —, — | no |
| requests-01 | 0 / 0 / 0 | 4 / 3 / 4 | pass/pass, pass/pass, pass/pass | yes |
| requests-02 | 0 / 1 / 0 | 3 / 3 / 5 | pass/pass, pass/pass, pass/pass | no |
| requests-03 | 0 / — / — | 7 / — / — | fail/pass, —, — | no |
| requests-04 | 3 / — / — | 6 / — / — | pass/pass, —, — | no |
| requests-05 | 0 / 0 / 0 | 2 / 2 / 4 | pass/pass, pass/pass, pass/pass | yes |
| requests-06 | 0 / — / — | 4 / — / — | fail/pass, —, — | no |
| requests-07 | 0 / 1 / 0 | 6 / 4 / 3 | pass/pass, pass/pass, pass/pass | no |
| requests-08 | 0 / — / — | 4 / — / — | fail/pass, —, — | no |
| ripgrep-01 | 0 / 0 / 0 | 4 / 4 / 4 | pass/pass, pass/pass, pass/pass | yes |
| ripgrep-02 | 0 / 0 / 0 | 5 / 3 / 3 | pass/pass, pass/pass, pass/pass | yes |
| ripgrep-03 | 2 / — / — | 2 / — / — | pass/pass, —, — | no |
| ripgrep-04 | 4 / — / — | 6 / — / — | fail/pass, —, — | no |
| ripgrep-05 | 0 / 0 / 1 | 2 / 2 / 2 | pass/pass, pass/pass, pass/pass | no |
| ripgrep-06 | 0 / — / — | 3 / — / — | fail/pass, —, — | no |
| ripgrep-07 | 2 / — / — | 6 / — / — | fail/pass, —, — | no |
| ripgrep-08 | 2 / — / — | 4 / — / — | pass/pass, —, — | no |

## Diagnostic oracle coverage

The separate offline run `ea0f78f4-public56-repaired` has **56 defined oracles**:
mean line coverage **6.17%**, mean inline coverage **2.68%**, pooled line
coverage **3.47%**, and commit recall **1/2** over the two annotated commit IDs.
See `coverage.json` for each numerator/denominator and the exact means.

This run uses the same frozen native binary and repaired questions:

```sh
python3 -I scripts/bench/tool-census/score_brief.py \
  --bin /tmp/pixel-906-ea0f78f4/pixel-frozen-home \
  --root /tmp/pixel-906-baseline --out /tmp/pixel-906-baseline/results \
  --label ea0f78f4-public56-repaired
```

This scorer uses a single-part CLI brief and semantic warm-up; it is not the
three-part live hook protocol. Its call count is not the agent call count.
This is a changed dataset, not an improvement over the historical 51-defined-
oracle baseline’s 5.81%. Line coverage is diagnostic and does not establish
answer correctness. In particular, broad oracle ranges dilute coverage, and
valid alternative Git commits are assessed during semantic review.

## Frozen-probe baseline for the #906 packs (2026-10-18, #1201)

The diagnostic run above used the `brief` command: a 750 ms window, no probe
floor and one part. Two runs of one binary then differ (54/56 identical briefs:
`config-registry` timed out at about 430 ms on one run of `pixel-03` and
`excalidraw-03`, not on the other), and the probes get far less room than the
live hook gives them. `score_brief.py` now builds the brief as the hook does
(`--route hook`, the default: 3 s window, probe floor, three parts) against a
home frozen from one commit (`freeze-home`, `--probe-home`), and records both.

Two runs of `main` `7887b9e6` on the probes frozen at that commit: **56/56
identical briefs, no metric differs**. This is the reference each pack
compares against (same binary, its probes frozen at the pack's commit):

| | command route | hook route (reference) |
|---|---:|---:|
| `file_recall` | 0.345 | **0.419** |
| `region_cov` | 0.052 | **0.084** |
| tests lines | 149/6 365 (2.3%) | **513/6 365 (8.1%)** |
| config lines | 90/979 (9.2%) | **371/979 (37.9%)** |
| docs lines | 85/3 488 (2.4%) | **320/3 488 (9.2%)** |
| `error-origin` `file_recall` | 0.154 | **0.186** |
| `flow` / `impact` line coverage | 6.7% / 6.2% | **6.8% / 11.2%** |

```sh
python3 -I scripts/bench/tool-census/score_brief.py pinned \
  --base <pixel built at 7887b9e6> --probe-commit 7887b9e6 \
  --label hook-7887b9e6 --root /tmp/pixel-906-baseline
# a pack: the same binary, --head-probe-commit <pack commit>
```

Binary SHA-256 `c41d123bfbd7d593354df2f705a28afb133a1065be1b1bf42bb2ba8ed025cc96`
(dev-release), question SHA-256 `984a632e…3c63e17069`, 26 frozen probe files.

### Pack results on the frozen-probe harness

Each row: the binary above, its probes frozen at `7887b9e6` (before) and at the
pack's commit (after), `score_brief.py pinned --head-probe-commit <commit>`.

| Pack | Commit | Targeted figure before → after | `file_recall` | `region_cov` | Verdict |
|---|---|---|---:|---:|---|
| #1196 `test-bodies` | `77f565ba` | tests lines 513 → 1 135 of 6 365 (8.1% → 17.8%); `tests` line coverage 9.6% → 21.1% | 0.419 → 0.420 | 0.084 → 0.087 | kept |
| #1197 `error-origin` (described messages) | `fce305a2` (vs `77f565ba`) | `error-origin` `file_recall` 0.186 → 0.244, line coverage 9.0% → 11.3% | 0.420 → 0.427 | 0.087 → 0.088 | kept |

## Remaining work

The next bounded prototypes should target the recorded failures, then be
tested on fresh cases before integration. This report does not implement them.

- Complete evidence for test questions: relevance admission, gitignore parent
  matching, history no-op/error contracts, redirect safeguards and sendFile policy.
- Preserve actual flow and impact boundaries: brief split/follow delivery,
  Pixel side-build exemptions, static canvas painting, cookie merges and fd output.
- Include the specific change or configuration exception requested: region
  context, wrap/unwrap semantics, asset URL correction, CGI proxy handling,
  removed Flask loaders, Windows color capability and suppressed error messages.
- Investigate repeated-case instability separately from incorrect answers.
  Unsuccessful controls require better evidence/review too; reducing calls alone
  is not the acceptance objective.

## Offline reproduction

From the repository root at this report’s commit:

```sh
d=docs/bench/brief-public-acceptance-2026-10-17
python3 -I scripts/bench/brief-zero-calls.py \
  --acceptance-run "$d/public56-ea0f78f4-1c2ed430-r1" \
  --acceptance-run "$d/public56-ea0f78f4-1c2ed430-r2" \
  --acceptance-run "$d/public56-ea0f78f4-1c2ed430-r3" \
  --quality-reviews "$d/reviews.jsonl" > /tmp/brief-public-acceptance.json
```

Expected: exit **1**, `accepted: 9`, `questions: 56`, `full_set_pass: false`,
`first_run_brief_calls: 78`, `first_run_control_calls: 235`. Compare the JSON
with committed `acceptance.json`. This command uses no model, credentials or
network. Live reruns are stochastic and need fresh review; identical answers
or exact timings are not promised. Elapsed times under concurrent execution
do not establish a causal speedup.
