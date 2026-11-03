# Mutation runner shutdown investigation — 2026-10-17

Refs [#1068](https://github.com/Pixel-CLI/pixel/issues/1068), following the
[nightly audit](mutants-nightly-audit-2026-10-17.md).

## What the retained evidence establishes

Five jobs explicitly report that their runner received a shutdown signal.
Four ran on GitHub-hosted runners; one ran on `b1`. The durations below are
job start to completion from the jobs API, not mutation execution time.
All five upload steps were **skipped**, despite `if: always()`.

| Scheduled run | Job / shard | Runner | UTC job window | Duration |
| --- | --- | --- | --- | --- |
| [37585948330](https://github.com/Pixel-CLI/pixel/actions/runs/37585948330) | [112676178698 / 3/15](https://github.com/Pixel-CLI/pixel/actions/runs/37585948330/job/112676178698) | `b1` | Oct 7, 09:44:20–09:58:53 | 14m 33s |
| [38520385957](https://github.com/Pixel-CLI/pixel/actions/runs/38520385957) | [115796427961 / 0/15](https://github.com/Pixel-CLI/pixel/actions/runs/38520385957/job/115796427961) | GitHub-hosted | Oct 14, 10:07:14–10:52:08 | 44m 54s |
| [38689277239](https://github.com/Pixel-CLI/pixel/actions/runs/38689277239) | [116365616648 / 0/15](https://github.com/Pixel-CLI/pixel/actions/runs/38689277239/job/116365616648) | GitHub-hosted | Oct 15, 10:08:17–10:55:24 | 47m 07s |
| same | [116365616659 / 2/15](https://github.com/Pixel-CLI/pixel/actions/runs/38689277239/job/116365616659) | GitHub-hosted | Oct 15, 10:08:18–11:17:01 | 68m 43s |
| same | [116365616690 / 14/15](https://github.com/Pixel-CLI/pixel/actions/runs/38689277239/job/116365616690) | GitHub-hosted | Oct 15, 10:08:18–10:27:39 | 19m 21s |

The four hosted jobs additionally report exit code 143. The `b1` job reports
an operation cancellation. Neither message identifies who or what stopped
the runner. Previously printed caught/missed outcomes remain in the logs;
the final in-flight mutant has no trustworthy verdict merely because the
runner stopped.

## Hypotheses checked against the actual campaign revisions

The campaign heads were `14feba456b479be165ab40ec14dde3958a865368`,
`fe0a1eab44420768a8b8a115fb91c4148292fd9f`, and
`75f92e92bd748d5dcc57d54544b202fa77be0405`, respectively. Their workflows
and cancellation script were inspected, rather than assuming today's files
describe historical behavior.

- **The shard's 90-minute job limit:** every affected job ended sooner.
- **Same-workflow concurrency replacement:** all three revisions set
  `cancel-in-progress: false` for `Mutants`.
- **The scheduled stale-PR sweep:** its `--closed` path selects only
  `pull_request` events and excludes `main`. These runs are `schedule` on
  `main`; they are not eligible. The close-event path selects a PR head.
- **Offline `b1` alone:** four affected jobs used GitHub-hosted runners.

Manual cancellation, a service stop, host resource exhaustion and platform
termination cannot be distinguished by these retained logs. In particular,
there is no basis here to label these incidents OOMs. The organization audit
API returned HTTP 404 to the current session; no audit attribution was
available. This does not establish whether an audit entry exists.

## Recovery boundary and next evidence

The upload is a later workflow step, not a durable checkpoint during mutation
execution. `always()` did not make that step run after these shutdowns.
Adding another identical final upload would not address the observed loss.
The current reporter and checkpoint reader reject missing outcomes; keep
that behavior. Do not fabricate JSON outcomes from a terminated process or
advance the campaign checkpoint to avoid replaying its unfinished range.

Keep #1068 open. For the four hosted jobs, platform-side lifecycle evidence
is needed, keyed by the job IDs and UTC windows above. For `b1`, the runner
service journal and host shutdown/resource logs for Oct 7, 09:44–09:59 UTC
are needed. Neither source was available in this investigation.

Any subsequent recovery change must demonstrate that completed outcomes
survive an interrupted runner and that the missing remainder still prevents
a checkpoint. A shorter shard budget (#1069) may limit lost work, but does
not establish the shutdown source or guarantee artifact durability. Use
ordinary fixture tests for recovery logic and the next scheduled campaign
for live validation; do not start a manual mutation campaign.

## Reproducing the inspection

For each run and linked job above:

```sh
gh api repos/Pixel-CLI/pixel/actions/runs/<run>
gh api --paginate 'repos/Pixel-CLI/pixel/actions/runs/<run>/jobs?per_page=100'
gh run view <run> --repo Pixel-CLI/pixel --job <job> --log
gh api 'repos/Pixel-CLI/pixel/actions/runs/<run>/artifacts?per_page=100'
git show <campaign-head>:.github/workflows/mutants.yml
git show <campaign-head>:.github/workflows/cancel-stale.yml
git show <campaign-head>:scripts/cancel-stale-runs.sh
```

The historical cancellation script lived under `scripts/`, before its move
to `scripts/ci/`. No workflow, runner setting, checkpoint or mutation result
was changed by this investigation.
