# Mutation nightly audit — 2026-10-17

Task: [#950](https://github.com/Pixel-CLI/pixel/issues/950). Read-only audit of all scheduled runs beginning 6 October. No mutation campaign was started or rerun. Source snapshot for the reporting fix: `f20f3b7f72851ac00626c76fa6be4baae5407756`.

## Recorded outcomes, not the listing

All ten completed runs are incomplete and retained their previous checkpoint. Their old report calls the listing “tested”, verdict `tested`; the authoritative completeness check and outcomes disagree. `Recorded` below includes caught/missed/timeout/unviable outcomes; it does not mean every candidate ran a test (unviable candidates did not compile). Ranges overlap and several heads repeat: **do not sum these rows as unique mutations**.

| UTC start | Run / head | Listed | Recorded | Caught | Missed | Timeout | Unviable | Missing | Classification |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| 2026-10-06 | [37430337823](https://github.com/Pixel-CLI/pixel/actions/runs/37430337823) / `4131611a` | 230 | 182 | 155 | 3 | 0 | 24 | 48 | 2 MISSED exit 2, 3 baseline failure |
| 2026-10-07 | [37585948330](https://github.com/Pixel-CLI/pixel/actions/runs/37585948330) / `14feba45` | 1947 | 1427 | 1179 | 79 | 1 | 168 | 520 | 10 MISSED exit 2, 2 baseline failure, 1 mutant TIMEOUT exit 3, 1 24h runner wait, 1 runner shutdown |
| 2026-10-08 | [37742996103](https://github.com/Pixel-CLI/pixel/actions/runs/37742996103) / `6edcfc21` | 3214 | 1064 | 845 | 94 | 0 | 125 | 2150 | 7 24h runner wait, 5 MISSED exit 2, 3 baseline failure |
| 2026-10-09 | [37898639070](https://github.com/Pixel-CLI/pixel/actions/runs/37898639070) / `85bede7d` | 3243 | 1073 | 855 | 91 | 3 | 124 | 2170 | 3 baseline failure, 7 24h runner wait, 1 mutant TIMEOUT exit 3, 4 MISSED exit 2 |
| 2026-10-10 | [38033161217](https://github.com/Pixel-CLI/pixel/actions/runs/38033161217) / `85bede7d` | 3243 | 1073 | 867 | 90 | 0 | 116 | 2170 | 7 24h runner wait, 5 MISSED exit 2, 3 baseline failure |
| 2026-10-11 | [38120486373](https://github.com/Pixel-CLI/pixel/actions/runs/38120486373) / `8a0f30e1` | 3243 | 1073 | 858 | 88 | 3 | 124 | 2170 | 3 baseline failure, 7 24h runner wait, 4 MISSED exit 2, 1 mutant TIMEOUT exit 3 |
| 2026-10-12 | [38217145954](https://github.com/Pixel-CLI/pixel/actions/runs/38217145954) / `8a0f30e1` | 3243 | 1073 | 866 | 80 | 3 | 124 | 2170 | 1 mutant TIMEOUT exit 3, 3 baseline failure, 7 24h runner wait, 4 MISSED exit 2 |
| 2026-10-13 | [38364266253](https://github.com/Pixel-CLI/pixel/actions/runs/38364266253) / `8a0f30e1` | 3243 | 856 | 680 | 75 | 0 | 101 | 2387 | 7 24h runner wait, 4 baseline failure, 4 MISSED exit 2 |
| 2026-10-14 | [38520385957](https://github.com/Pixel-CLI/pixel/actions/runs/38520385957) / `fe0a1eab` | 7331 | 2737 | 2074 | 420 | 2 | 241 | 4594 | 7 24h runner wait, 4 90m job limit, 2 MISSED exit 2, 1 mutant TIMEOUT exit 3, 1 runner shutdown |
| 2026-10-15 | [38689277239](https://github.com/Pixel-CLI/pixel/actions/runs/38689277239) / `75f92e92` | 8394 | 2157 | 1699 | 316 | 2 | 140 | 6237 | 7 24h runner wait, 3 90m job limit, 3 runner shutdown, 1 MISSED exit 2, 1 mutant TIMEOUT exit 3 |
| 2026-10-16 | [38857843148](https://github.com/Pixel-CLI/pixel/actions/runs/38857843148) / `31bc6fc2` | — | — | — | — | — | — | — | 7 queued b1, 8 MISSED exit 2 |

All completed gate summaries report 0 disk-full outcomes. The 16 October run is still queued; its completed hosted shard annotations say exit 2, but GitHub withholds its raw logs until the run completes, so aggregate counts are unavailable. No totals are inferred from those annotations.

## Distinct causes and disposition

- **Offline runner capacity:** the repository variable is `["ubuntu-26.04","b1"]`. The runners API reports the only `b1` runner offline. Seven odd-numbered shards of each 8–15 October run expired with the explicit annotation `The job has exceeded the maximum execution time while awaiting a runner for 24h0m0s`; seven `b1` shards of 16 October remain queued. Thus the 14 October queue was waiting for a runner; it finished failed on 15 October. Tracked by [#1067](https://github.com/Pixel-CLI/pixel/issues/1067).

- **Runner shutdowns:** 7 October shard 3/15, 14 October shard 0/15, and 15 October shards 0/15, 2/15, 14/15 log a runner shutdown signal after passing baselines. The latter hosted jobs exit 143. This is distinct from the queue and job timeout. Tracked by [#1068](https://github.com/Pixel-CLI/pixel/issues/1068).

- **90-minute job limit:** four shards on 14 October and three on 15 October have GitHub annotations that the job exceeded 1h30m0s. The backlog grew to 8,394 listed mutants (about 560 per shard at the 15-shard cap). This is not the same as a per-mutant `TIMEOUT` outcome. Tracked by [#1069](https://github.com/Pixel-CLI/pixel/issues/1069).

- **Real survivors:** every completed run has MISSED outcomes; several also have TIMEOUT outcomes. They remain test debt even after execution infrastructure is repaired. Retain their exact mutation names from each gate log/artifact and reconcile against current main; the counts are not additive. Tracked by [#1070](https://github.com/Pixel-CLI/pixel/issues/1070).

- **6 October baseline failure, already repaired:** `core::output_limit_accepts_exact_cap_and_rejects_each_overflowing_stream` failed the machine-speed assertion `receipt.duration_ms < 1_500` on three shards. [#815](https://github.com/Pixel-CLI/pixel/pull/815), commit `f779c688`, replaced the wall-clock assumption with an observable process-continuation marker on 6 October after that run. No new issue is needed for that historical assertion.

- **7–13 October baseline failure, already repaired:** `prompt_brief_cli::a_stale_graph_should_be_named_and_never_rebuilt_by_the_hook` saw a base-shard mtime change in its before/after tuple. [#887](https://github.com/Pixel-CLI/pixel/pull/887), commit `b5baa235`, gives each fixture a different committed tree to prevent shared-shard cache collisions. That fix is an ancestor of the 14 October head; no baseline failure appears in the inspected 14–15 October shard logs.

- **Misleading reporting, fixed with this audit:** a listing alone now says `planned`; incomplete/invalid outcome sets say `incomplete`; only complete recognized outcomes say `tested`. Listed counts and recorded outcomes are separate. A complete MISSED/TIMEOUT campaign remains red but may checkpoint; an incomplete or disk-full campaign remains red and cannot checkpoint. The runner pool, job limits and cumulative selection policy are unchanged.

## Evidence and repeatable reads

```sh
gh run list --repo Pixel-CLI/pixel --workflow mutants.yml --limit 20 \
  --json databaseId,headSha,event,status,conclusion,createdAt,url
gh api repos/Pixel-CLI/pixel/actions/runs/<run>/jobs?per_page=100
gh run view <run> --repo Pixel-CLI/pixel --job <job-id> --log
gh api repos/Pixel-CLI/pixel/check-runs/<job-id>/annotations
gh variable get PIXEL_MUTANTS_SHARD_RUNNERS --repo Pixel-CLI/pixel
gh api repos/Pixel-CLI/pixel/actions/runners
```

Counts come from the **Mutants in diff** job log of each completed run; its completeness-check message explicitly reports the missing outcomes and retained checkpoint. Cause classifications use failed shard logs and check-run annotations, not just red step names. The audit checked every shard’s metadata/annotations and downloaded available failed-shard logs. Runner state and queue counts are observations at this audit, not permanent assertions about capacity.

Representative evidence:
- [37430337823: Mutants shard 11/15, baseline failure](https://github.com/Pixel-CLI/pixel/actions/runs/37430337823/job/112159672580).
- [37585948330: Mutants shard 3/15, runner shutdown](https://github.com/Pixel-CLI/pixel/actions/runs/37585948330/job/112676178698).
- [38520385957: Mutants shard 9/15, 24h runner wait](https://github.com/Pixel-CLI/pixel/actions/runs/38520385957/job/115796427803).
- [38520385957: Mutants shard 8/15, 90m job limit](https://github.com/Pixel-CLI/pixel/actions/runs/38520385957/job/115796427832).
- [38689277239: Mutants shard 0/15, runner shutdown](https://github.com/Pixel-CLI/pixel/actions/runs/38689277239/job/116365616648).
