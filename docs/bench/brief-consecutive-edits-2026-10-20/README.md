# Briefs during consecutive edits — 2026-10-20

Issue #1203; follow-up to merged PRs #1253, #1254, #1269 and #1270.
The original 0/20 failure improved but was still reproducible on current
main: **18/20 hybrid briefs**, versus **20/20 with this fix**. v1 remains
20/20. This is hook availability, not a live-agent answer-quality result.

## Frozen inputs

- Baseline: `d3cb0ddeb082e85e6c860c8d88b1eebc89229827` (main including #1270).
- Candidate: `48b55a69e0428ba9e9a4f2ee6664f6b63916620c`.
- Both built with `cargo build --profile dev-release -p pixel-cli`.
- Gumroad: `b996417e6fe55b984a8a36b1a2780ef0ca742724`, shallow clone,
  separate disposable copies for v1 and `PIXEL_GRAPH_SOURCE_RUBY=hybrid`.
- Same historical prompt seed, five edited paths, hook command, isolated
  HOME/XDG, no installed probes in the Gumroad arms. Public benchmark probes
  are frozen separately from `c6cd78edf42272d401295935174862d6a069dd09`.
- `identity.json` records binary/dataset/probe identities. `base-hooks.jsonl`
  and `head-hooks.jsonl` retain hook measurements, stdout hashes and publication checks;
  `gumroad-summary.json` contains aggregates.

## Gumroad results

Every arm has 20 edits, a prompt sent one second after each edit, and two
later prompts. A usable brief requires a successful hook, nonzero brief
bytes and at least one answered operation. The original report counted
nonzero brief bytes; both criteria agree on these runs.

| Measure | Main v1 | Fixed v1 | Main hybrid | Fixed hybrid |
|---|---:|---:|---:|---:|
| Briefs one second after edit | 20/20 | 20/20 | 18/20 | **20/20** |
| Hook p50 / p95 (ms), one second after edit | 368 / 2257 | 281 / 2075 | 304 / 3021 | 283 / 3019 |
| Later briefs | 40/40 | 40/40 | 40/40 | 40/40 |
| Later hook p50 / p95 (ms) | 384 / 746 | 429 / 642 | 379 / 1495 | 331 / 1598 |
| Early briefs with the staleness notice | 18/20 | 20/20 | 18/20 | 20/20 |
| Edited method published after cycle | 20/20 | 20/20 | 20/20 | 20/20 |

All measured hooks used `route: daemon`. Main's hybrid failures were prompt
indices 3 and 33: `answered: 0`, `bytes: 0`, stdout `{}`, elapsed 3001 ms
inside the brief. The fixed run answers both. A first one-cycle scratch
replay had passed on main; only consecutive edits exposed the residual gap.

The high hybrid tail still reaches the brief's 3 s window (plus hook process
overhead). This change restores content availability; it does not establish
that every requested operation completes, nor solve hybrid refresh cost
(#1204). Nearest-rank quantiles, one run per binary, alternating v1/hybrid
order by cycle. Shared-machine load median was 13.1 before and 10.3 after;
no local compilation ran during either timed campaign. The latency figures
are descriptive, not a statistically established speedup. v1's observed
p50/p95 are below both this main baseline and the historical #1202 result.

## Reproduce

Use disposable clones only: the historical harness resets tracked files,
removes their `.pixel`, and recreates its isolated homes. Save the two built
binaries as `$RUN/base-pixel` and `$RUN/head-pixel`; clone Gumroad at the pin
above into `$RUN/v1` and `$RUN/hybrid` outside ignored directory names such
as `.cache`, `build` and `vendor`.

```sh
D=docs/bench/brief-consecutive-edits-2026-10-20
H=docs/bench/ruby-hybrid-daemon-2026-10-18/daemonbench.py
python3 -I "$D/replay.py" "$H" "$RUN/base-pixel" "$RUN/v1" "$RUN/hybrid" "$RUN/base" 20
python3 -I "$D/summarize.py" "$RUN/base"
python3 -I "$D/replay.py" "$H" "$RUN/head-pixel" "$RUN/v1" "$RUN/hybrid" "$RUN/head" 20
python3 -I "$D/summarize.py" "$RUN/head"
```

The wrapper reuses the historical preparation, edits, deterministic prompts
and hook sequence. It omits #1204's separate active/passive freshness timing
campaigns and checks each edit's publication once after the cycle instead.
It saves raw hook stdout/stderr and exits nonzero for a missing brief or
unpublished edit. The baseline exited 1; the candidate exited 0. An initial
candidate invocation stopped before any measurement on two orphaned Git
index locks in the disposable clones. No process held them; they were
removed and the recorded run used a new output directory.

## Public 56-question control

Both arms rendered **56/56 usable briefs, byte-identical for every question**.
Mean oracle-line coverage is unchanged at **13.06%**, inline coverage 10.74%,
and file recall 45.75%; zero partial briefs in both arms. Elapsed p95 is
1111 → 1059 ms (one pair, no speedup claim). `public-questions.jsonl` retains
pins, output hashes and per-question metrics; `public-summary.json` retains
aggregates. `diff -rq` on the two 56-file `briefs/` directories exited 0.
This measures fresh-repository oracle coverage and latency, not
stale-after-edit behavior, live quality or zero-call acceptance.

```sh
python3 -I scripts/bench/tool-census/score_brief.py freeze-home \
  --commit c6cd78ed --out "$RUN/probe-home"
python3 -I scripts/bench/tool-census/score_brief.py pinned \
  --base "$RUN/base-pixel" --head "$RUN/head-pixel" \
  --probe-home "$RUN/probe-home" --root "$RUN/public" \
  --out "$RUN/public-scores" --label issue1203
```

## Diagnosis and regression

`refresh_files` advanced the publication generation without publishing a
matching `PublishedSnapshot`. `read_answer` therefore returned `None` during
a subsequent update and `serve_read` handed the request back to the writer.
The existing single-batch tests explicitly warmed the snapshot with a writer
request; they could not catch this sequence. The fix stages a compact
snapshot outside the lock, then publishes it under the same lock as the
index/graph generation. HEAD/repository-write withdrawal stays unchanged.

`GIT_CONFIG_GLOBAL=/dev/null cargo test --profile dev-release -p pixel-daemon
--lib stale_brief_should_answer_during_a_second_batch_without_a_writer_read`
fails before the fix, passes after, and fails again with only the production
fix temporarily removed:

```text
the second batch must leave the previous snapshot readable
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 444 filtered out
```

After restoration: `cargo fmt --all`, `cargo clippy -p pixel-daemon
--all-targets -- -D warnings`, and `GIT_CONFIG_GLOBAL=/dev/null cargo test
--profile dev-release -p pixel-daemon` pass: 444 unit tests, 32 integration
tests, 1 ignored. Ordinary read-after-write tests still pass. No local
installation, mutation campaign, live agent or refresh-cost campaign ran.
