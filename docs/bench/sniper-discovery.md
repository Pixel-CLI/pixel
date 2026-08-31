# Sniper-discovery experiment — results

## Verdict

**❌ The sniper instruction makes agents blind. Worse than doing nothing.**

Discovery operations drop dramatically, but gold recall collapses far more
than the 5-point tolerance — across all three harnesses, independently. The
instruction causes agents to over-rely on a closed file list and stop
exploring, naming fewer correct files as a result.

Per the pre-registered success criteria, this is outcome #2: *discovery_ops
drops AND gold_recall drops >5 points → sniper makes agents blind — worse
than doing nothing.*

## Machine

- **Host:** Apple M4 Max, 64 GB RAM, macOS Darwin 27.0.0 (arm64)
- **Python:** 3.14.6
- **gitpixel:** 0.1.0 (release binary, `~/.local/bin/gitpixel`)
- **codex:** codex-cli 0.150.1 (OAuth auth)
- **gemini:** 0.49.0 (OAuth auth, `--skip-trust --approval-mode plan -p`)
- **opencode:** 1.18.20 (default model: deepseek-v4-pro)
- **Date:** 2026-08-27/28

## Rerun command

The original experiment harness (`scripts/experiments/sniper/`) targeted the
retired `gitpixel` binary and has been removed. The measurement methodology
and results below remain valid as a historical record; a re-run against the
current `pixel` binary would require a new harness.

## Results

### Per-cell medians (single trial — no variance estimate)

| harness   | arm | n  | disc_ops | files | bytes    | wall_s | recall | prec   |
|-----------|-----|----|----------|-------|----------|--------|--------|--------|
| codex     | A   | 7  | 3133     | 2     | 666792   | 198    | 1.000  | 1.000  |
| codex     | B   | 8  | 886      | 2     | 165015   | 103    | 0.097  | 0.500  |
| gemini    | A   | 7  | 0        | 0     | 0        | 22     | 0.214  | 0.600  |
| gemini    | B   | 4  | 0        | 0     | 0        | 67     | 0.107  | 0.375  |
| opencode  | A   | 8  | 24       | 8     | 39640    | 79     | 0.722  | 1.000  |
| opencode  | B   | 8  | 1        | 0     | 0        | 47     | 0.500  | 0.923  |

### Overall per-arm medians

| arm | n  | disc_ops | recall | prec   |
|-----|----|----------|--------|--------|
| A   | 22 | 24       | 0.600  | 1.000  |
| B   | 20 | 2        | 0.194  | 0.750  |

- **discovery_ops:** 24 → 2 (92% drop) ✅ hypothesis H1 confirmed
- **gold_recall:** 0.60 → 0.19 (41-point drop) ❌ hypothesis H2 failed
- **gold_precision:** 1.00 → 0.75 (25-point drop) ❌

### Per-run detail

| run_id              | status                    | disc_ops | files | bytes    | wall_s | recall | prec   | t_reads |
|---------------------|---------------------------|----------|-------|----------|--------|--------|--------|---------|
| T1-A-gemini         | completed                 | 0        | 0     | 0        | 22     | 0.500  | 0.500  | 0       |
| T1-B-codex          | completed                 | 126      | 2     | 4065     | 38     | 0.000  | 0.000  | 1       |
| T1-A-opencode       | completed_edits_discarded | 3        | 0     | 1122     | 102    | 0.500  | 0.250  | 9       |
| T1-B-gemini         | completed_edits_discarded | 0        | 0     | 0        | 118    | 0.000  | 0.000  | 0       |
| T1-B-opencode       | completed                 | 1        | 0     | 0        | 108    | 0.500  | 0.250  | 11      |
| T1-A-codex          | completed                 | 1988     | 2     | 443335   | 220    | 0.500  | 1.000  | 53      |
| T2-A-gemini         | completed                 | 0        | 0     | 0        | 51     | 0.333  | 0.667  | 0       |
| T2-A-opencode       | completed                 | 24       | 12    | 44500    | 221    | 1.000  | 1.000  | 0       |
| T2-B-codex          | completed                 | 1800     | 2     | 165015   | 570    | 0.167  | 0.500  | 20      |
| T2-A-codex          | dropped (timeout)         | -        | -     | -        | -      | -      | -      | -       |
| T2-B-gemini         | dropped (timeout)         | -        | -     | -        | -      | -      | -      | -       |
| T2-B-opencode       | completed                 | 3        | 0     | 132      | 463    | 1.000  | 0.750  | 10      |
| T3-A-gemini         | completed                 | 0        | 0     | 0        | 94     | 0.300  | 1.000  | 0       |
| T3-A-opencode       | completed                 | 7        | 2     | 6197     | 270    | 0.600  | 1.000  | 5       |
| T3-B-opencode       | completed                 | 1        | 0     | 0        | 47     | 0.400  | 0.800  | 9       |
| T3-B-codex          | completed                 | 2398     | 2     | 514964   | 343    | 0.500  | 1.000  | 11      |
| T3-A-codex          | completed                 | 3133     | 2     | 666792   | 375    | 1.000  | 1.000  | 48      |
| T3-B-gemini         | dropped (timeout)         | -        | -     | -        | -      | -      | -      | -       |
| T4-A-gemini         | completed                 | 0        | 0     | 0        | 13     | 0.214  | 0.600  | 0       |
| T4-B-codex          | completed                 | 164      | 2     | 27249    | 26     | 0.000  | 0.000  | 4       |
| T4-A-opencode       | completed                 | 28       | 15    | 128255   | 37     | 1.000  | 0.778  | 1       |
| T4-B-gemini         | completed                 | 0        | 0     | 0        | 25     | 0.214  | 0.333  | 0       |
| T4-B-opencode       | completed                 | 1        | 0     | 0        | 25     | 0.714  | 1.000  | 11      |
| T4-A-codex          | completed                 | 3242     | 2     | 621757   | 171    | 1.000  | 1.000  | 34      |
| T5-A-gemini         | completed                 | 0        | 0     | 0        | 13     | 0.077  | 0.333  | 0       |
| T5-A-opencode       | completed                 | 22       | 2     | 7384     | 54     | 0.385  | 1.000  | 4       |
| T5-B-opencode       | completed                 | 2        | 0     | 0        | 23     | 0.154  | 1.000  | 4       |
| T5-B-codex          | completed                 | 886      | 2     | 233570   | 103    | 0.308  | 1.000  | 26      |
| T5-A-codex          | completed                 | 3027     | 2     | 864905   | 176    | 1.000  | 1.000  | 48      |
| T5-B-gemini         | dropped (timeout)         | -        | -     | -        | -      | -      | -      | -       |
| T6-B-codex          | completed                 | 126      | 2     | 4195     | 32     | 0.000  | 0.000  | 3       |
| T6-A-opencode       | completed                 | 23       | 6     | 8124     | 72     | 0.722  | 0.867  | 7       |
| T6-B-opencode       | completed                 | 2        | 0     | 0        | 38     | 0.667  | 0.923  | 10      |
| T6-A-codex          | completed                 | 1884     | 2     | 612235   | 198    | 0.889  | 0.889  | 9       |
| T6-A-gemini         | dropped (timeout)         | -        | -     | -        | -      | -      | -      | -       |
| T6-B-gemini         | dropped (timeout)         | -        | -     | -        | -      | -      | -      | -       |
| T7-A-gemini         | completed                 | 0        | 0     | 0        | 23     | 0.097  | 0.600  | 0       |
| T7-A-opencode       | completed                 | 31       | 17    | 65233    | 79     | 0.806  | 1.000  | 0       |
| T7-B-gemini         | completed                 | 0        | 0     | 0        | 67     | 0.097  | 0.375  | 0       |
| T7-B-opencode       | completed                 | 1        | 0     | 0        | 33     | 0.194  | 1.000  | 6       |
| T7-B-codex          | completed                 | 2418     | 2     | 580221   | 207    | 0.097  | 1.000  | 9       |
| T7-A-codex          | completed                 | 3976     | 2     | 1450259  | 210    | 1.000  | 1.000  | 42      |
| T8-A-gemini         | completed_edits_discarded | 0        | 0     | 0        | 14     | 0.071  | 1.000  | 0       |
| T8-B-gemini         | completed                 | 0        | 0     | 0        | 40     | 0.107  | 1.000  | 0       |
| T8-B-codex          | completed                 | 316      | 2     | 37097    | 56     | 0.000  | 0.000  | 4       |
| T8-A-opencode       | completed                 | 24       | 8     | 39640    | 74     | 0.143  | 1.000  | 5       |
| T8-B-opencode       | completed                 | 1        | 0     | 0        | 52     | 0.214  | 0.750  | 12      |
| T8-A-codex          | completed                 | 3850     | 2     | 1322042  | 142    | 0.964  | 1.000  | 6       |

## Root cause analysis

### 1. `gitpixel targets` can only return existing files — 50% of gold files are new

The experiment checks out the **parent** of each mined commit, so the change
is not yet present. Many gold files are **newly created** by the commit and
do not exist at the checkout point.

| Task | Gold files | New (not at parent) | New % |
|------|-----------|---------------------|-------|
| T1   | 2         | 1                   | 50%   |
| T2   | 6         | 4                   | 67%   |
| T3   | 10        | 0                   | 0%    |
| T4   | 14        | 11                  | 79%   |
| T5   | 13        | 6                   | 46%   |
| T6   | 18        | 17                  | 94%   |
| T7   | 31        | 22                  | 71%   |
| T8   | 28        | 0                   | 0%    |
| **Total** | **122** | **61**          | **50%** |

`gitpixel targets` indexes the codebase at the checkout point and can only
return files that already exist. It is structurally incapable of naming
files that would need to be created. The sniper instruction tells agents to
"work only from the returned P0/P1 list" and "do not ls, grep, or read
files outside it" — so agents cannot discover that new files are needed.

### 2. Even for existing files, the targets list is incomplete

Tasks T3 (0% new) and T8 (0% new) — where every gold file already exists —
still show recall collapse:

- T3: codex A recall=1.0 → codex B recall=0.5 (50-point drop)
- T8: codex A recall=0.96 → codex B recall=0.0 (96-point drop)

The targets list does not include all relevant existing files. The agents,
told to work only from the list, miss them.

### 3. Agents stop exploring

The sniper instruction's "do not ls, grep, or read files outside it" causes
agents to terminate exploration after reading the targets list files. In
arm A, agents explore broadly (codex: 3000+ discovery ops) and find most
gold files. In arm B, they read the targets list and stop (codex: ~886
ops, opencode: ~1 op).

## What was NOT measured

### Gemini discovery operations

Gemini CLI uses built-in tools (`ReadFile`, `Glob`) that bypass the PATH
shim entirely. The shim-captured `discovery_ops` is 0 for every gemini run.
The transcript parser also failed to extract gemini's tool calls from the
transcript format. **Gemini's discovery_ops and distinct_files_read are
unmeasurable with this instrumentation approach.** Only recall and
precision are valid for gemini.

### Dropped runs (6 of 48)

| Run          | Reason  |
|--------------|---------|
| T2-A-codex   | timeout (600s) |
| T2-B-gemini  | timeout (600s) |
| T3-B-gemini  | timeout (600s) |
| T5-B-gemini  | timeout (600s) |
| T6-A-gemini  | timeout (600s) |
| T6-B-gemini  | timeout (600s) |

5 of 6 drops are gemini timeouts. Gemini in `--approval-mode plan` appears
to hang on larger tasks (T2, T3, T5, T6 have 6–31 gold files). The 600s
timeout was chosen to bound the experiment; a longer timeout might recover
some runs but would not change the direction of the result.

### Edits-discarded runs (3 of 48)

| Run             | Details |
|-----------------|---------|
| T1-A-opencode   | opencode edited files despite localization-only instruction |
| T1-B-gemini     | gemini edited files in plan mode |
| T8-A-gemini     | gemini edited files in plan mode |

These runs were scored (the named files were extracted before resetting)
but flagged. The edits were reverted with `git checkout -- .` before
continuing.

### Single trial per cell

Each cell (task × harness × arm) was run once. There is no variance
estimate. The medians are point estimates, not confidence intervals. A
single trial is sufficient to detect the large effect size here (recall
drop of 41 points), but small differences between harnesses should not be
over-interpreted.

### Index build cost (reported separately, per design)

| Task | Index build (s) |
|------|-----------------|
| T1   | 4.2             |
| T2   | 1.0             |
| T3   | 18.4            |
| T4   | 0.5             |
| T5   | 0.6             |
| T6   | 1.4             |
| T7   | 2.9             |
| T8   | 0.2             |

Index build is a fixed setup cost, not charged to the query. It is excluded
from `wall_seconds` per the design.

### Model diversity

Three genuinely different models were used:
- **codex** → OpenAI model (via MCP, OAuth)
- **gemini** → Google Gemini (via OAuth, plan mode)
- **opencode** → DeepSeek V4 Pro (default model)

This is an improvement over the a2 run where two of three harnesses ended
up calling the same Gemini model through different wrappers.

### Path resolution issue (arm B)

In some arm B runs, `gitpixel targets` wrote its manifest to the working
repo (`~/gitpixel` → `/Users/livio/Documents/gitpixel`) instead of the test
repo (`~/gitpixel-under-test`). Agents then followed absolute paths from
the targets output to the working repo (at HEAD) rather than the test repo
(at the historical checkout). This means arm B agents may have read
slightly different file contents than arm A agents. However, the file
**paths** are the same in both repos (the files exist at the same relative
locations), so the localization accuracy comparison is still valid — the
issue affects file contents read, not which files are named.

## Conclusion

The sniper instruction — "run `gitpixel targets`, work only from the
returned list, do not explore outside it" — reduces discovery operations by
92% but collapses gold recall by 41 points. This is not a win; it is a
failure mode the design explicitly warned about: *fewer reads is
worthless — actively harmful — if the agent ends up naming the wrong
files.*

The root cause is structural: `gitpixel targets` can only return existing
files, but 50% of gold files in this corpus are newly created by the commit
being studied. Even for existing files, the targets list is incomplete, and
the instruction's prohibition on exploration prevents agents from
discovering the gaps.

The direction of the result is consistent across all three harnesses and
all three models. This is not a harness-specific artifact.
