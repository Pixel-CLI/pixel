# Conditional configuration loader evidence — #1233, 2026-10-19

Verdict: **proved bounded coverage improvement**. One generic Python source
mechanism adds an environment-named file loader's conditional overwrite chain.
It does not establish the active deployment mode, the actual invocation order,
or an effective configuration value. #1199 remains CLOSED / COMPLETED; #1233
remains open for those runtime and binding questions. No merge is requested.

## Hypotheses and source review

1. **Dropped:** quote a dotenv dictionary-merge loader, its file order and
   `load_defaults` mode. The scratch script correctly exposed Flask CLI's
   conditional file order and preserved ambient environment, but `flask-03`
   asks about application configuration. Replaying this addition changed
   neither line_cov (0.188259109) nor region_cov (0.007816511). No production
   dotenv rule was ported; no effective value was inferred.
2. **Validated:** following a dynamic environment-name lookup forwarded to a
   same-class file helper through to a mapping write supplies the missing
   conditional overwrite evidence. This adds information that a registry of
   literal environment keys cannot express. A scratch AST script preceded the
   probe edit; the corrected single-question replay predicted the native gain.

At Flask pin `d086db85` (full pin in identity.json), `src/flask/config.py`
initializes defaults at :94-100, obtains the filename dynamically at :114,
forwards it with `silent` at :124, reads/executes the file at :204-209, handles
missing files conditionally at :210-214, calls the object loader at :215, and
writes uppercase attributes into the mapping at :250-254. These are candidate
same-class source paths, not proof that a particular instance invokes them.
Successful later assignments replace existing keys; a missing file in silent
mode returns without those assignments. `silent` is an error-handling mode,
**not** development/production selection. No environment variable or user
configuration file is evaluated by the probe.

Controls cover both loader classes sharing method names, the existing two
DEFAULT_TIMEOUT scopes, a positive chain, a disconnected env read, a missing
writer and an unrelated tour intent. Comments/docstrings, string and numeric
literals and f-string text cannot leak through the new source lane. Generic
`key` identifiers remain visible so the assignment guard is not erased.
Quoted blocks cover only emitted rows, excluding omitted docstrings/gaps.

## Frozen native comparison

The unchanged corrected **protocol v4** reference follows #1230/#1235/#1239.
The baseline reproduces all 56 reference coverage rows. It retains #1243's
homonym fix: overlay its program onto the common frozen bundle. A second
overlay changes only that same program to this candidate. The other 27 probe
files remain fixed; unrelated newer probes on main are excluded. These are
measurement-only Git objects, not branches or additional PR units.

- Source checkout: `c347dccae2a97868a8f7185f97fa65cacb31171c`.
- Binary source: `a0d99d6268b770b49bf44effef415ac95b1252aa`; SHA-256
  `fcde2a41d4d0e7017acdba2ee58e4803afa9379b8b3995b68e24b71ec14d8e64`.
- Scorer SHA-256: `95bf349407a86091b7627f0d7726697478408b743345b32d4ed919da36233106`.
- Questions SHA-256: `984a632e96f5f32891279d58f872c3a891bfeabaee110b3993be313c63e17069`.
- Baseline probes: `6ddc7c7e539ab5fdaae13a08e5db01dad10a3f07`.
- Candidate probes: `5b471c995783e50240a9e626117f6263327720f4`; program SHA-256
  `f1cb1d961f4b80f1c5de67b7e6dbd4f4292164e577f0ee7581a63f242e8d7fb1`.
- Same seven repository pins, hook Pi brief_only, warm, three parts / 3000 ms,
  process timeout 300 s, dedupe off. Private clones; sequential arms under
  `/tmp/pixel-benchmark.lock`. Both reports complete/exit **0**: 53 ok + 3
  abstained each, no errors/cuts/partial rows. Abstentions remain denominators.

Exact command, executed while holding that lock (exit **0**):

```sh
/Users/navid/.local/share/mise/installs/python/3.15.0/bin/python3 -I scripts/bench/tool-census/score_brief.py pinned --base /tmp/pixel-1233-precedence/pixel --head /tmp/pixel-1233-precedence/pixel --probe-commit 6ddc7c7e539ab5fdaae13a08e5db01dad10a3f07 --head-probe-commit 5b471c995783e50240a9e626117f6263327720f4 --repo /Users/navid/.codex/worktrees/906-1199-followup/pixel --questions /Users/navid/.codex/worktrees/906-1199-followup/pixel/eval/brief-bench/questions.jsonl --label precedence56 --route hook --root /tmp/pixel-1233-precedence/state --out /tmp/pixel-1233-precedence/runs
```

Full identities, expanded argv, 28 hashes per arm, reconstructable overlay tree
and program blob, verified repository pins and process results:
[identity.json](identity.json). To reconstruct either overlay, use a temporary
`GIT_INDEX_FILE`, `git read-tree a0d99d62`, replace only
`probes/config-registry/probe` with the recorded program blob, then
`git write-tree` / `git commit-tree`. Verify every probe hash before reuse.

| Metric | Baseline | Candidate | Delta |
|---|---:|---:|---:|
| Config oracle type (19 questions) | 225/979 | 225/979 | 0 |
| Config category line_cov (7 questions) | 0.100974142 | 0.109360492 | +0.008386350 |
| Config category region_cov | 0.085233228 | 0.085545214 | +0.000311986 |
| All file_recall | 0.425377968 | 0.425377968 | 0 |
| All region_cov | 0.084869149 | 0.084908147 | +0.000038998 |
| All line_cov | 0.110799104 | 0.111847398 | +0.001048294 |
| flask-03 line_cov / inline_cov | 0.188259109 | 0.246963563 | +0.058704453 |
| flask-03 region_cov | 0.007816511 | 0.010000411 | +0.002183899 |

Only **flask-03** changes text or coverage; the other **55/56** questions and
all other categories are unchanged. **No measured coverage regression**.
The oracle's source-type classification credits these new rows as definition
evidence; config-question-category gain is distinct from config-oracle-type
coverage. All deltas: [per-question.jsonl](per-question.jsonl),
[per-category.jsonl](per-category.jsonl). The gain includes masked source rows
as the existing protocol permits; the important lookup, mode condition,
forwarding and mapping assignment themselves were reviewed as visible source.

## Checks and limits

`python3 -I probes/config-registry/tests/test_probe.py`: **20 passed**, exit 0.
With `PROBE_UNDER_TEST=/tmp/pixel-1233-precedence/baseline-probe` pointing to
the program at source checkout above: **4 failures**, exit 1. The same suite
retains tracked-only `.env`, symlink, masking and homonym regressions.
`python3 -I scripts/bench/probe-test.py --check-all`: exit 0; its `--self-test`
passes **24** tests. Overfit guard, Python AST compilation, SPDX and whitespace
checks pass. Native pair and test logs remain under the run directory above.
The existing probe harness also passes four synthetic cases with its budget,
determinism, read-only and merge checks: `PROBE config-registry | loader,beta,positive | predicted calls 1 -> 0 | coverage 67% -> 100% | OK`.
This is an offline oracle-completeness verdict; the predicted counts are
placeholders, not observed agent calls or a live `KEEP`.

No real-agent run, paired answer-quality review, tool-call reduction, zero-call
or performance claim. No Rust/scorer edit or local installation. No full-current
main probe-combination claim beyond the frozen bundle measured here. The AST
lane's caps and unsupported forms are documented in the probe README. Runtime
dispatch, imports/aliases, multi-stage data flow, actual deployment mode, later
calls and complete override precedence remain unknown. The earlier rejected
`config-sites` lexical enclosing-body rule has not been revived.
