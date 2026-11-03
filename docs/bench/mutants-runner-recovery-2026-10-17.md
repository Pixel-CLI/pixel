# Nightly runner-pool recovery — 2026-10-17

Refs [#1067](https://github.com/Pixel-CLI/pixel/issues/1067), following the
[mutation nightly audit](mutants-nightly-audit-2026-10-17.md).

## Applied configuration

The live repository variable `PIXEL_MUTANTS_SHARD_RUNNERS` changed from
`["ubuntu-26.04","b1"]` to `["ubuntu-26.04"]`. The runners API still reported
the only `b1` runner offline. This restores the workflow's GitHub-hosted
capacity for future plans without changing its runner-selection algorithm.

```sh
gh variable get PIXEL_MUTANTS_SHARD_RUNNERS --repo Pixel-CLI/pixel
gh api repos/Pixel-CLI/pixel/actions/runners
gh variable set PIXEL_MUTANTS_SHARD_RUNNERS --repo Pixel-CLI/pixel \
  --body '["ubuntu-26.04"]'
```

The value was read back successfully after the write. The production
`scripts/ci/mutants-gate.py` planner, fed the live value and the retained
481-candidate listing, maps all 15 shards to `ubuntu-26.04`. This validates
the current configuration and planner, not a future scheduled run.

## Disposition of the old queue

Scheduled run [38857843148](https://github.com/Pixel-CLI/pixel/actions/runs/38857843148),
head `31bc6fc2f2ff69d392797894ec2ca4223ffbb90b`, had seven queued `b1`
shards and no job in progress. The completed shards and plan had already
uploaded nine artifacts. Their metadata and contents were downloaded before
cancelling the blocked run:

```sh
gh run download 38857843148 --repo Pixel-CLI/pixel \
  --pattern 'mutants-*' --dir <evidence-directory>
gh run cancel 38857843148 --repo Pixel-CLI/pixel
```

After cancellation, the run is `completed` / `cancelled`, with no queued
jobs. All nine original artifact IDs remain available in GitHub:

| Artifact | ID |
| --- | --- |
| `mutants-out-0` | `12039808632` |
| `mutants-out-10` | `12039204159` |
| `mutants-out-12` | `12039099005` |
| `mutants-out-14` | `12039053193` |
| `mutants-out-2` | `12039169896` |
| `mutants-out-4` | `12040085032` |
| `mutants-out-6` | `12040660794` |
| `mutants-out-8` | `12039079474` |
| `mutants-plan` | `12039755847` |

The production `count_mutants` and `tally` readers report **481 listed,
250 recorded outcomes: 181 caught, 31 missed, 38 unviable**. The remaining
231 candidates lack a recorded outcome. These are partial results, not a
complete campaign or passing coverage. They are not additive with the
preceding cumulative nightlies.

The repository had zero `mutants-nightly-checkpoint` artifacts both before
and after cancellation. No checkpoint or cumulative-range setting was
changed. The existing selector rejects cancelled runs as checkpoint sources;
the next scheduled campaign must still cover the unjudged range. No mutation
campaign was dispatched or rerun manually.

## Verification still owed

Keep #1067 open until the next **scheduled** `Mutants` run is observed using
the hosted runner pool with no `b1` assignments. A repository-variable edit
does not retarget jobs that were already queued, which is why the old run
was explicitly cancelled after preserving evidence. Do not manually dispatch
or rerun a mutation campaign for this check.

This restores available capacity only. Runner shutdowns (#1068), campaign
sizing (#1069) and survivor tests (#1070) remain separate work; changing the
pool does not establish that the next campaign will complete or pass.
