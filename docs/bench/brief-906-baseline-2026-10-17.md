# Issue #906: pinned public baseline, 2026-10-17

This establishes the current-binary oracle-coverage baseline requested by
[#906](https://github.com/Pixel-CLI/pixel/issues/906). It does **not** establish
13/13 zero-call answers or equal-answer quality. The original private census
is unavailable on this machine; these are the separate public 56 questions
on seven pinned repositories, not a replacement for the historical census.

## Identity and reproduction

- Pixel source: `ea0f78f444ff6d6d8fa2327e06548523edf00cd3`, version 0.8.0,
  `dev-release`, default features, aarch64-apple-darwin, rustc 1.99.0.
- Native binary SHA-256:
  `ce1b238d3cd58e2cf1cefd3e8dedf64f1983afcc6618b3b45735d3da084b4a23`.
- Questions: `eval/brief-bench/questions.jsonl`, SHA-256
  `872155d43134ea0c7b8838730829badf82b177aa5adf41e7c4e86a99a981f2f9`.
- [Machine-readable receipt](brief-906-baseline-2026-10-17.jsonl): identity,
  bundled probe hashes, live pilot counts/answer hashes, then one row per public
  question with its full repository SHA and raw coverage metrics.
- No user configuration or installed programs were changed. Pixel alone ran
  with a scratch home containing the 13 bundled probes from the same commit;
  the live agent used a disposable `CODEX_HOME` and the existing login.

From a clean checkout at the source SHA above, build and freeze the binary
and the exact bundled probe set (the wrapper changes the Pixel process's home,
not the operator's shell or the live agent's login):

```sh
cargo build --profile dev-release -p pixel-cli
python3 - <<'PYCODE'
from pathlib import Path
import re, shlex, shutil
frozen = Path('/tmp/pixel-906-ea0f78f4')
frozen.mkdir(exist_ok=True)
shutil.copy2('target/dev-release/pixel', frozen / 'pixel')
bundled = Path('crates/pixel-setup/src/bundled_probes.rs').read_text()
for source in re.findall(r'include_str!\("../../../(probes/[^"\n]+)"\)', bundled):
    relative = Path(source).relative_to('probes')
    target = frozen / 'home/.pixel/plugins' / relative
    target.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(source, target)
wrapper = frozen / 'pixel-frozen-home'
wrapper.write_text('#!/bin/sh\nexec env HOME=' + shlex.quote(str(frozen / 'home'))
                   + ' ' + shlex.quote(str(frozen / 'pixel')) + ' "$@"\n')
wrapper.chmod(0o755)
PYCODE
python3 -I scripts/bench/tool-census/score_brief.py \
  --bin /tmp/pixel-906-ea0f78f4/pixel-frozen-home \
  --root /tmp/pixel-906-baseline --out /tmp/pixel-906-baseline/results \
  --label ea0f78f4-public56-frozen
```

The scorer prepares each pinned clone, requests a semantic warm-up and settles
the brief. This is its **single-part CLI brief** protocol; it does not call
`build-index --history` or prove live three-part delivery. The live pilot below
uses the hook protocol and builds history. These protocols are not comparable
byte-for-byte or interchangeable coverage and zero-call claims.

An initial exploratory run used the ordinary home and was discarded as a
current-main baseline: installed `cand-k-commit-patch` and `history-fallback`
programs differed from main. The receipt records only the rerun with the frozen
set. Native binary identity alone would not have pinned the executed probes.

## Coverage result

All 56 invocations exited 0 and rendered nonempty briefs. Five questions have
empty oracle arrays in the public dataset: `requests-03`, `flask-04`,
`flask-05`, `express-04`, `express-05`. Their coverage is undefined, not zero
or complete. Means exclude undefined values, so line coverage has **51**
measured questions. No final-answer correctness is measured by this scorer.

Mean per-question line coverage: **5.81%**, inline coverage: **2.00%**,
file recall: **29.59%**. Pooled line coverage is **3.10%**
(total covered lines / total oracle lines); it weights large oracles more heavily.

| session | category | mode | brief B | oracle lines | line cov | inline cov | region cov | file recall | answer cov | gate |
|---|---|---|---:|---:|---:|---:|---:|---:|---:|---|
| pixel-01 | overview | answer | 3869 | 988 | 6% | 5% | 8% | 75% | n/a | open |
| pixel-02 | lookup | answer | 3611 | 365 | 5% | 4% | 2% | 2% | n/a | open |
| pixel-03 | config | answer | 2141 | 616 | 1% | 0% | 1% | 6% | n/a | open |
| pixel-04 | flow | answer | 3439 | 2841 | 0% | 0% | 0% | 3% | n/a | open |
| pixel-05 | history | answer | 3877 | 42 | 0% | 0% | 0% | 36% | n/a | open |
| pixel-06 | tests | answer | 3877 | 898 | 0% | 0% | 0% | 3% | n/a | open |
| pixel-07 | error-origin | answer | 3286 | 746 | 0% | 0% | 0% | 0% | n/a | open |
| pixel-08 | impact | answer | 3694 | 2734 | 0% | 0% | 0% | 0% | n/a | open |
| ripgrep-01 | overview | answer | 3736 | 294 | 0% | 0% | 0% | 60% | n/a | open |
| ripgrep-02 | lookup | answer | 2920 | 835 | 6% | 6% | 0% | 4% | n/a | open |
| ripgrep-03 | config | answer | 1944 | 183 | 1% | 0% | 2% | 45% | n/a | open |
| ripgrep-04 | flow | answer | 3668 | 2519 | 7% | 2% | 1% | 22% | n/a | open |
| ripgrep-05 | history | answer | 3848 | 1 | 0% | 0% | 0% | 0% | n/a | open |
| ripgrep-06 | tests | answer | 3490 | 820 | 0% | 0% | 0% | 17% | n/a | open |
| ripgrep-07 | error-origin | answer | 1619 | 1700 | 1% | 1% | 0% | 11% | n/a | open |
| ripgrep-08 | diagnosis | answer | 2858 | 539 | 0% | 0% | 2% | 16% | n/a | open |
| requests-01 | overview | answer | 3526 | 2205 | 0% | 0% | 3% | 78% | n/a | open |
| requests-02 | lookup | answer | 1257 | 823 | 18% | 0% | 2% | 17% | n/a | open |
| requests-03 | config | answer | 3620 | 0 | n/a | n/a | n/a | n/a | n/a | open |
| requests-04 | flow | answer | 3401 | 1213 | 0% | 0% | 0% | 0% | n/a | open |
| requests-05 | history | answer | 2326 | 3 | 67% | 0% | 67% | 100% | n/a | open |
| requests-06 | tests | answer | 3732 | 1130 | 5% | 3% | 0% | 12% | n/a | open |
| requests-07 | error-origin | answer | 1213 | 234 | 0% | 0% | 0% | 50% | n/a | open |
| requests-08 | impact | answer | 2945 | 1082 | 5% | 5% | 0% | 12% | n/a | open |
| excalidraw-01 | overview | answer | 2466 | 1168 | 4% | 4% | 0% | 2% | n/a | open |
| excalidraw-02 | lookup | answer | 3810 | 28 | 4% | 0% | 4% | 21% | n/a | open |
| excalidraw-03 | config | answer | 1343 | 108 | 0% | 0% | 0% | 0% | n/a | open |
| excalidraw-04 | flow | answer | 2898 | 3331 | 0% | 0% | 0% | 0% | n/a | open |
| excalidraw-05 | history | answer | 3921 | 162 | 1% | 0% | 1% | 18% | n/a | open |
| excalidraw-06 | tests | answer | 3897 | 1091 | 1% | 0% | 1% | 50% | n/a | open |
| excalidraw-07 | error-origin | answer | 528 | 846 | 0% | 0% | 0% | 0% | n/a | open |
| excalidraw-08 | impact | answer | 3740 | 180 | 0% | 0% | 0% | 5% | n/a | open |
| flask-01 | overview | answer | 3598 | 392 | 14% | 0% | 3% | 17% | n/a | open |
| flask-02 | lookup | answer | 3080 | 62 | 6% | 3% | 5% | 19% | n/a | open |
| flask-03 | config | answer | 2272 | 494 | 0% | 0% | 0% | 9% | n/a | open |
| flask-04 | flow | answer | 2910 | 0 | n/a | n/a | n/a | n/a | n/a | unjudged |
| flask-05 | history | answer | 3871 | 0 | n/a | n/a | n/a | n/a | n/a | closed |
| flask-06 | tests | answer | 3848 | 558 | 2% | 0% | 0% | 71% | n/a | open |
| flask-07 | error-origin | answer | 2170 | 471 | 0% | 0% | 1% | 15% | n/a | open |
| flask-08 | impact | answer | 1539 | 1455 | 1% | 0% | 0% | 3% | n/a | open |
| fd-01 | overview | answer | 3563 | 1269 | 4% | 0% | 50% | 90% | n/a | open |
| fd-02 | lookup | answer | 3775 | 152 | 26% | 26% | 7% | 25% | n/a | open |
| fd-03 | config | answer | 3181 | 321 | 12% | 5% | 6% | 19% | n/a | open |
| fd-04 | flow | answer | 3567 | 1387 | 17% | 5% | 11% | 29% | n/a | open |
| fd-05 | history | answer | 2460 | 6 | 17% | 0% | 17% | 100% | n/a | open |
| fd-06 | tests | answer | 3780 | 615 | 10% | 6% | 1% | 29% | n/a | open |
| fd-07 | error-origin | answer | 2659 | 213 | 21% | 16% | 4% | 30% | n/a | open |
| fd-08 | impact | answer | 3750 | 90 | 31% | 11% | 29% | 58% | n/a | open |
| express-01 | overview | answer | 2515 | 1373 | 1% | 0% | 1% | 100% | n/a | open |
| express-02 | lookup | answer | 1268 | 198 | 1% | 0% | 1% | 58% | n/a | open |
| express-03 | config | answer | 1250 | 568 | 1% | 0% | 0% | 54% | n/a | open |
| express-04 | flow | answer | 1677 | 0 | n/a | n/a | n/a | n/a | n/a | open |
| express-05 | history | answer | 2882 | 0 | n/a | n/a | n/a | n/a | n/a | closed |
| express-06 | tests | answer | 3745 | 987 | 1% | 0% | 8% | 40% | n/a | unjudged |
| express-07 | error-origin | answer | 3809 | 250 | 2% | 0% | 2% | 2% | n/a | open |
| express-08 | impact | answer | 1237 | 208 | 0% | 0% | 0% | 75% | n/a | open |

## Missed oracle regions by type (pooled over all sessions)

| type | oracle items | covered | coverage | quoted inline | sessions with it | sessions >= half covered |
|---|---:|---:|---:|---:|---:|---:|
| definition bodies (code read) | 27627 | 854 | 3% | 339 | 39 | 0 |
| usage / search-hit lines | 2293 | 88 | 4% | 19 | 40 | 1 |
| tests | 6494 | 149 | 2% | 71 | 34 | 0 |
| config | 955 | 90 | 9% | 90 | 17 | 0 |
| docs | 3425 | 85 | 2% | 0 | 38 | 3 |
| git (commit ids + touched files) | 0 | 0 | n/a | 0 | 0 | 0 |
| listed files | 0 | 0 | n/a | 0 | 0 | 0 |

## Live pilot: measurement, not an acceptance campaign

```sh
CODEX_BIN="$(command -v codex)" python3 -I scripts/bench/brief-zero-calls.py \
  --brief-bin /tmp/pixel-906-ea0f78f4/pixel-frozen-home \
  --only requests-02 --arm both --root /tmp/pixel-906-baseline \
  --run-id ea0f78f4-requests02-frozen --timeout 180
```

Codex 0.163.0, `gpt-5.6-terra`, medium effort, one run per arm, three hook parts:
**3 calls without the brief, 2 with it**. All three parts ran, 24,318 bytes
were delivered, brief generation took 360 ms, and both arms produced a nonempty
answer. The runner exited 1 because the brief arm still fetched evidence.
The historical public baseline is 4 calls; it is not the paired control.

Manual inspection found both answers described the same core timeout behavior
at the pinned Requests source: `timeout=None`, resolved to `connect=None` and
`read=None`, so no Requests-enforced connect/read deadline. This is one content
check, not a blinded quality review or a claim about all answers/citations.
Private raw streams remain under the run root, outside Git; the receipt carries
only counts and answer hashes. This single pair proves neither a reliable
call reduction nor zero-call success. Preliminary ordinary-home pilot results
are excluded from the final baseline too.

## Remaining acceptance work for #906

| Area | Already implemented | What this baseline still requires |
|---|---|---|
| Delivery | Multi-hook parts (#907), lost-part follow-up (#1035 / #1063) | Measure delivered bytes and part count for every live case; never credit undelivered runs |
| Code regions / flow | Region packs (#909), chain and code-site probes in `probes/` | Definition coverage remains low; translate the remaining live reads before changing ranking |
| Settings / tests | Locators and kind packs (#913), `config-registry` | Repair the five empty oracle records before treating all 56 as coverage evidence |
| Gate / recall | Relevance gate (#911), admission follow-up #1032 | Record missed required regions separately from optional context; single-part coverage does not establish full-hook recall |
| History | `history-fallback`, `cand-k-commit-patch` | Pin their deployed code; use a history-enabled protocol for history acceptance |
| Answer quality | Delivery/call instrumentation, now nonempty-answer checks | Review paired final answers against the pinned source; hashes identify answers but do not judge them |
| Historical target | #905 private census protocol | Recover that private dataset or explicitly agree a new acceptance set; do not silently substitute public questions |

The harness now labels zero calls as **quality unreviewed**, rejects empty
answers and unsuccessful Claude result events, and ignores explicit Codex
commentary when looking for a final answer. Its exit status remains a call-count
and delivery check, not a semantic-quality gate. #906 stays open pending the
coverage fixes and a repeated equal-answer acceptance campaign.
