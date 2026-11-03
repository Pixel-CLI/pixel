# #1234: documentation admission, 2026-10-19

**KEEP for the bounded selection/formatting change; partial follow-up, not closure of #1234.**
#1200 remains closed. This is a development-set comparison, not held-out validation or an answer-quality/zero-call claim.

## Frozen comparison

The integrated reference is [the #1201 integration audit](https://github.com/Pixel-CLI/pixel/issues/906#issuecomment-6257964366), published in #1246.
Binary source **dbff5b09**, SHA-256 `20bc41f6c9f2767733589ee6852e47c8400301f4835a8ad2edf996dda3c00988`; scorer **brief-oracle v4**, SHA-256 `95bf349407a86091b7627f0d7726697478408b743345b32d4ed919da36233106`.
The question SHA-256, seven pins, all 28 probe-file hashes per arm and parameters are in `identity.json`.
Candidate probes are frozen at **a12f774e76547a685a5d0c5bed8614d8e60ea35d**. Only `doc-sections/probe` and its manifest differ; 26 other files are identical.
The same binary runs both arms sequentially under `/tmp/pixel-benchmark.lock`, in private clones, with warm Pi brief_only hooks, 3 parts / 3000 ms, dedupe off, process timeout 300 s.
Both runs exit **0**, `complete`: **53 ok + 3 abstained**, zero error/partial/native-cut rows. Abstentions stay in the denominator of 56.
The new baseline's four metric vectors/types match the integrated reference for all **56/56 questions**. No latency improvement is claimed.

| Metric | Baseline | Candidate |
|---|---:|---:|
| file_recall | 0.424004341 | 0.419341809 |
| region_cov | 0.086780779 | 0.086295897 |
| line_cov | 0.114340248 | 0.114219802 |
| inline_cov | 0.097366457 | 0.091293630 |
| docs oracle covered | 371/3488 | 367/3488 |
| docs oracle quoted | 364/3488 | 359/3488 |
| documentation blocks | 54 | 32 |
| repeated documentary rows | 11 | 0 |
| unmarked shortened documentary rows | 26 | 0 |

The four lost covered oracle rows are `pixel-02` (1) and `ripgrep-06` (3): respectively a historical experiment/review-design excerpt instead of current brief-cap constants, and the guide's binary-data section instead of gitignore tests.
The old 371-line acceptance threshold is **not met**; no threshold is silently lowered and #1234 is not declared complete. This candidate's proven benefits are the negative controls, novelty and explicit cuts, with these recall losses disclosed.
`requests-05` also loses one quoted oracle row while both covered rows and the commit evidence remain; inline_cov 1/3 → 0. No equally complete answer is inferred.
File recall decreases on pixel-02/04/06/08 and ripgrep-06 as documentary paths and the code/configuration paths they mention disappear. All 56 metric pairs, source-header lists, status/size, lost oracle lines and outside-oracle row counts are in `per-question.jsonl`; nine category aggregates are in `per-category.jsonl`.

## Controls and source review

- `pixel-02` and `fd-02`: **2 → 0 documentation sections each**. The veto uses generic kind/source-identifier intent; no benchmark IDs, repository names or answer literals occur in the probe.
- Positive controls preserve **16/16** proxy docs oracle rows (`requests-03`), **8/209** timeout rows (`requests-02`) and **7/7** application-context rows (`flask-07`). The last count is documentation only, not a repair of the integration audit's other lost code evidence.
- Existing quoted rows are removed by `(path,line)`, independent of header labels. Partial overlaps produce contiguous subranges with exact headers; a removed duplicate is not replaced with a lower-ranked section.
- The **32 candidate docs blocks** were checked against their pins: exact paths/ranges and consecutive numbered rows, source-equal text or an exact 160-character prefix followed by ` … [truncated]`; zero source mismatches. Seven shortened rows are explicitly marked. A 121-row excerpt also announces any unquoted tail.
- **No code source rows are lost in any of the 56 questions**, including outside the oracle. This does not establish that all removed docs were useless: the history/contribution context of `requests-05` and overview/impact docs deserve live inspection.
- **Outside-oracle navigation hints do disappear:** `pixel-04` loses an unquoted `crates/pixel/src/main.rs:1891` pointer, plus script paths; `pixel-07` loses the `crates/pixel-git/tests/boundary.rs` path; `pixel-08` loses `Cargo.lock`, `crates/pixel-daemon/src/api.rs` and `daemon.rs` names. These can be useful exploration clues. Per-question `named_files_lost` and `code_pointer_coordinates_lost` record every loss; zero lost code *quotes* does not mean zero lost code evidence. Their answer-quality cost is unverified.
- `flask-04`: current baseline and candidate both quote **0/18** rows of `src/flask/debughelpers.py:64-81`. Even a native pilot with docs entirely removed still quotes 0/18: changing docs alone does not restore a block absent from the integrated producer. Do not add it back as an unsupported call chain.
- On the original saved audit packet where the block is present, the current probe preserves **18/18 → 18/18**, with zero quoted rows lost; fixture SHA-256 `c0a2b33c61c51333aaf26dc9ec5ffb4c8d46aaddf794452836c1cd58462ba613`. This is a historical budget-preservation control, not a current production or semantic-completeness claim.
- The separate Express path defect #1245 remains unchanged. Source-fidelity validation here is for documentary excerpts; it does not certify every header of the full native packet.

## Hypotheses

Rejected: restricting docs to overview/config only. Scratch replay: **371 → 352** docs rows, including loss of the useful timeout/application-context controls. Rejected separately: removing docs would restore the missing current flask-04 block; native **0/18 → 0/18** disproves that premise.
Kept: veto judged source navigation and unclassified intent, and explicit code identifiers outside overview/config, while preserving other explanatory kinds. The initial generic `formula` literal failed the overfit guard and was removed; unclassified intent now abstains instead. Final guard: **56/56 prompts pass**.
The range and truncation repairs address known output-contract defects, rather than adding a second ranking heuristic. Ranking, file scan and section/line limits are unchanged.

## Reproduction and validation

Use the exact integrated binary above; do not rebuild or install it. `SOURCE` is a clean dbff5b09 checkout, `CONTENT_REPO` holds the candidate commit, and `ROOT`/`OUT` are fresh private run directories. The source prepares the seven pinned repositories. Hold the shared lock throughout:

```sh
python3 -c 'import fcntl,subprocess,sys; f=open("/tmp/pixel-benchmark.lock","a"); fcntl.flock(f,fcntl.LOCK_EX); sys.exit(subprocess.call(sys.argv[1:]))' \
  python3 -I "$SOURCE/scripts/bench/tool-census/score_brief.py" pinned \
  --base "$BINARY" --probe-commit dbff5b09d35a00a07572751d556b330a9da38acd \
  --head-probe-commit a12f774e76547a685a5d0c5bed8614d8e60ea35d --repo "$CONTENT_REPO" \
  --questions "$SOURCE/eval/brief-bench/questions.jsonl" --route hook \
  --root "$ROOT" --out "$OUT" --label final-source-intent-v4
```

`python3 -I probes/doc-sections/tests/test_probe.py`: **3 pass**. `python3 -I probes/doc-sections/tests/test_admission.py`: **8 pass**; the same new tests against the dbff5b09 probe give **10 failed assertions**, exit 1.
`probe-test.py --check-all`: pass; `probe-test.py --self-test`: **24 pass**; Python compilation, SPDX and diff whitespace checks pass. The normal `probe-test --guard` command first lacked its default binary; the guard was then evaluated directly with the harness's `overfit` function on all 56 frozen prompts, without changing the checker.
Local `scripts/ci/gates.sh`: the inherited tool PATH first broke an unrelated fake-cargo fixture (101 instead of its stub's 0/7). With the direct Python/system-tool PATH and empty BASH_ENV, all script gates pass; its Rust path filter skips Cargo on this probe-only diff. CI must validate the final PR head; no local mutation campaign, install or live agent run.
Next live checks: `pixel-04/07/08` (lost navigation hints), `requests-05` (lost history/contribution quote), `pixel-08` (impact docs removed), `flask-04` and `flask-07` (existing completeness gaps), paired with proxy/timeout positives and pixel-02/fd-02 negatives. Keep model/effort equal; zero-call acceptance requires three reviewed paired repetitions.
