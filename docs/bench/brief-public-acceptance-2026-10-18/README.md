# Public brief acceptance evaluation — 2026-10-18

**Verdict: 18/56 accepted (2026-10-17: 9/56); #906 remains open.** Same 56
questions, same protocol and the same Codex model as the
[2026-10-17 evaluation](../brief-public-acceptance-2026-10-17/README.md), on
`main` `904d9570`, after the six packs of #1208–#1215. This measures the
binary at that commit, with its probes frozen, not one pack.

## Results

| Run | Questions | Brief calls | Fresh control calls | Brief quality failures | Control quality failures |
|---|---:|---:|---:|---:|---:|
| r1 | 56 | 64 | 231 | 12 of 34 reviewed | 0 of 34 reviewed |
| r2 | 22 | 3 | 84 | 1 of 19 reviewed | 0 of 19 reviewed |
| r3 | 22 | 1 | 80 | 0 of 19 reviewed | 0 of 19 reviewed |

- **First pass:** 64 brief calls against 231 control calls (2026-10-17: 78
  against 235). 34 of 56 brief answers used no tool call (34 on 2026-10-17).
- **Delivery:** every brief arrived in all three parts, none late, in all 100
  brief sessions. First-pass brief latency: p50 223 ms, p95 891 ms, max
  1 240 ms, against a 3 000 ms window.
- **Review of r1:** of the 34 zero-call pairs, 22 passed source review on both
  arms. They are the provisional candidates, against 17 on 2026-10-17. The 22
  pairs with brief calls were not reviewed: a brief call already rules a case
  out.
- **Repeats:** the 22 candidates were run twice more.
  - Three needed a tool on a repeat: `requests-05` and `ripgrep-01` in r2,
    `requests-07` in r3.
  - Of the other 19, `fd-06` failed review in r2: its brief answer
    misdescribes `test_exec_with_separator` (it tests `--path-separator`, not
    the `;` separator).
  - That leaves 18 accepted: three complete pairs with zero brief calls,
    passing quality on both arms and full delivery.
- **The control never failed review** in the 72 reviewed pairs. The brief
  arm's failures in r1 were mostly incomplete answers:
  - test suites half covered (`requests-06`, `ripgrep-06`, `excalidraw-06`,
    `pixel-06`);
  - a commit named but not what it changed (`fd-05`, `excalidraw-05`);
  - a flow missing its callers (`flask-08`, `requests-08`, `pixel-08`);
  - a message or a variable left out (`pixel-07`, `fd-03`).

  Two also say something false. `express-01` gives `params` to the request
  prototype, which the router fills. `pixel-08` cites a file that does not
  exist at the pin.

## Frozen inputs and method

- Pixel source: `904d95709cd92d577033827ccffb5a44d939f33e` (dev-release).
  Native SHA-256: `d7a30cb7…14d6ab`; the wrapper that points `HOME` at the
  frozen probe home: `51379d7c…cae21`.
- Probes: the 14 bundled probes of `904d9570`, frozen by
  `score_brief.py freeze-home --commit 904d9570`. Each file's hash is in
  `identity.json`.
- Question SHA-256 `984a632e…63e17069`, the same file as on 2026-10-17.
- Agent: Codex 0.163.0, `gpt-5.6-terra`, medium effort.
- Runner settings: three hook parts, four session workers, two index workers,
  a 600-second session timeout, history enabled. Harness source: `904d9570`.
- The control had no Pixel hooks. Each pair ran in fresh workspaces at the
  question's pin.

```sh
CODEX_BIN=$(command -v codex) python3 -I scripts/bench/brief-zero-calls.py --brief-bin <frozen-home wrapper> \
  --agent codex --arm both --jobs 4 --index-jobs 2 --timeout 600 --parts 3 --run-id public56-904d9570-r1 --root <root>
# r2 and r3: the same, with --run-id public56-904d9570-r2/-r3 and --only <the 22 r1 candidates>
python3 -I scripts/bench/brief-zero-calls.py --acceptance-run public56-904d9570-r1 \
  --acceptance-run public56-904d9570-r2 --acceptance-run public56-904d9570-r3 \
  --quality-reviews reviews.jsonl > acceptance.json   # exit 1: the full set does not pass
```

- **A first r1 attempt is not reported.** All 112 of its sessions errored
  before Codex started: the runner looked for `codex` only in
  `~/.local/bin`, and this machine's is a mise install. The runner now takes
  the `codex` on `PATH` and refuses to start when none is found.
- **Reviews** (`reviews.jsonl`, 72 rows) are source reviews by Claude
  (Opus 5.5) sub-agents, four per round. Each review is bound to both answer
  hashes, the full source pin and the pinned ranges it checked. They are
  neither blinded nor independent human review. A different reviewer from
  2026-10-17, which used Codex, so the two quality columns are not strictly
  comparable.
- **What is published:** the reports hold counts, tools, bytes, timings and
  answer hashes. Raw answers and streams stay local, as on 2026-10-17.

## Per-question verdicts

`B/C` is brief/control source-review quality. `unreviewed` is a pair with
brief calls in r1 (not a candidate); `—` is a pair not repeated.

| Question | Brief calls r1/r2/r3 | Control calls r1/r2/r3 | Quality B/C r1, r2, r3 | Accepted |
|---|---|---|---|---|
| excalidraw-01 | 3 / — / — | 7 / — / — | unreviewed, —, — | no |
| excalidraw-02 | 0 / 0 / 0 | 2 / 1 / 2 | pass/pass, pass/pass, pass/pass | yes |
| excalidraw-03 | 2 / — / — | 5 / — / — | unreviewed, —, — | no |
| excalidraw-04 | 10 / — / — | 9 / — / — | unreviewed, —, — | no |
| excalidraw-05 | 0 / — / — | 5 / — / — | fail/pass, —, — | no |
| excalidraw-06 | 0 / — / — | 5 / — / — | fail/pass, —, — | no |
| excalidraw-07 | 1 / — / — | 3 / — / — | unreviewed, —, — | no |
| excalidraw-08 | 2 / — / — | 4 / — / — | unreviewed, —, — | no |
| express-01 | 0 / — / — | 2 / — / — | fail/pass, —, — | no |
| express-02 | 1 / — / — | 2 / — / — | unreviewed, —, — | no |
| express-03 | 4 / — / — | 5 / — / — | unreviewed, —, — | no |
| express-04 | 3 / — / — | 4 / — / — | unreviewed, —, — | no |
| express-05 | 1 / — / — | 4 / — / — | unreviewed, —, — | no |
| express-06 | 0 / 0 / 0 | 3 / 5 / 3 | pass/pass, pass/pass, pass/pass | yes |
| express-07 | 3 / — / — | 4 / — / — | unreviewed, —, — | no |
| express-08 | 2 / — / — | 4 / — / — | unreviewed, —, — | no |
| fd-01 | 0 / 0 / 0 | 3 / 3 / 5 | pass/pass, pass/pass, pass/pass | yes |
| fd-02 | 0 / 0 / 0 | 4 / 5 / 4 | pass/pass, pass/pass, pass/pass | yes |
| fd-03 | 0 / — / — | 3 / — / — | fail/pass, —, — | no |
| fd-04 | 4 / — / — | 5 / — / — | unreviewed, —, — | no |
| fd-05 | 0 / — / — | 2 / — / — | fail/pass, —, — | no |
| fd-06 | 0 / 0 / 0 | 3 / 5 / 6 | pass/pass, fail/pass, pass/pass | no |
| fd-07 | 2 / — / — | 3 / — / — | unreviewed, —, — | no |
| fd-08 | 0 / 0 / 0 | 5 / 5 / 6 | pass/pass, pass/pass, pass/pass | yes |
| flask-01 | 0 / 0 / 0 | 3 / 3 / 4 | pass/pass, pass/pass, pass/pass | yes |
| flask-02 | 0 / 0 / 0 | 1 / 2 / 3 | pass/pass, pass/pass, pass/pass | yes |
| flask-03 | 0 / 0 / 0 | 3 / 3 / 2 | pass/pass, pass/pass, pass/pass | yes |
| flask-04 | 1 / — / — | 5 / — / — | unreviewed, —, — | no |
| flask-05 | 0 / 0 / 0 | 4 / 2 / 2 | pass/pass, pass/pass, pass/pass | yes |
| flask-06 | 0 / 0 / 0 | 3 / 3 / 5 | pass/pass, pass/pass, pass/pass | yes |
| flask-07 | 2 / — / — | 4 / — / — | unreviewed, —, — | no |
| flask-08 | 0 / — / — | 6 / — / — | fail/pass, —, — | no |
| pixel-01 | 0 / 0 / 0 | 4 / 6 / 3 | pass/pass, pass/pass, pass/pass | yes |
| pixel-02 | 0 / 0 / 0 | 3 / 5 / 3 | pass/pass, pass/pass, pass/pass | yes |
| pixel-03 | 0 / 0 / 0 | 6 / 4 / 3 | pass/pass, pass/pass, pass/pass | yes |
| pixel-04 | 11 / — / — | 8 / — / — | unreviewed, —, — | no |
| pixel-05 | 1 / — / — | 3 / — / — | unreviewed, —, — | no |
| pixel-06 | 0 / — / — | 5 / — / — | fail/pass, —, — | no |
| pixel-07 | 0 / — / — | 7 / — / — | fail/pass, —, — | no |
| pixel-08 | 0 / — / — | 8 / — / — | fail/pass, —, — | no |
| requests-01 | 0 / 0 / 0 | 3 / 4 / 3 | pass/pass, pass/pass, pass/pass | yes |
| requests-02 | 0 / 0 / 0 | 4 / 3 / 4 | pass/pass, pass/pass, pass/pass | yes |
| requests-03 | 0 / 0 / 0 | 2 / 5 / 3 | pass/pass, pass/pass, pass/pass | yes |
| requests-04 | 0 / 0 / 0 | 7 / 5 / 4 | pass/pass, pass/pass, pass/pass | yes |
| requests-05 | 0 / 1 / 0 | 2 / 3 / 2 | pass/pass, unreviewed, unreviewed | no |
| requests-06 | 0 / — / — | 4 / — / — | fail/pass, —, — | no |
| requests-07 | 0 / 0 / 1 | 3 / 4 / 4 | pass/pass, unreviewed, unreviewed | no |
| requests-08 | 0 / — / — | 3 / — / — | fail/pass, —, — | no |
| ripgrep-01 | 0 / 2 / 0 | 4 / 5 / 4 | pass/pass, unreviewed, unreviewed | no |
| ripgrep-02 | 0 / 0 / 0 | 5 / 3 / 5 | pass/pass, pass/pass, pass/pass | yes |
| ripgrep-03 | 2 / — / — | 5 / — / — | unreviewed, —, — | no |
| ripgrep-04 | 3 / — / — | 7 / — / — | unreviewed, —, — | no |
| ripgrep-05 | 1 / — / — | 2 / — / — | unreviewed, —, — | no |
| ripgrep-06 | 0 / — / — | 4 / — / — | fail/pass, —, — | no |
| ripgrep-07 | 2 / — / — | 4 / — / — | unreviewed, —, — | no |
| ripgrep-08 | 3 / — / — | 3 / — / — | unreviewed, —, — | no |
