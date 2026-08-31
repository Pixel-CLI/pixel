# Pixel Hijack Map

30-day mining of the operator's agent-operation archive (2026-07-27 → 2026-08-26): 21,700 raw
operations sent to three coding agents (codex 16,736 · devin 3,023 · a dictation-driven
assistant 1,941), deduplicated to 7,250 unique operations, then semantically triaged through
10 parallel lanes. Goal: rank the repetitive actions pixel should deterministically hijack,
with verbatim evidence, and derive the new-op backlog from measured demand instead of
intuition.

All personal, organizational, and project identifiers are anonymized (`<org>`, `<project>`,
`PROJ-123`, "a teammate"); operation ids refer to a private local archive and are opaque
without it. Verbatim quotes are otherwise unedited, profanity included — anger density is
part of the signal.

## Verdict

The demand is real and concentrated: git mutations (`ship`), history recovery
(`rescue`/`excavate`), and transcript recall dwarf everything else. Of the five original
MANDATORY scenarios, three are backed by heavy user demand and two (`targets`, `impact`)
have **zero** demand-side evidence in 30 days. The biggest wins for aggressive hijacking are
not new rules but:

1. making `ship` the fast default for the #1 chain (bare "commit and push"),
2. closing the force-push / squash / amend gap that forced raw git,
3. widening trigger phrasing to the vocabulary users actually produce — often
   voice-transcribed and oblique ("get it back", "like it was before", "githy story" as a
   transcription mangle of "git history").

## Demand ranking — corrected, not raw

Raw regex counts are inflated 2–8× by orchestrator prompts, dictation noise, and pasted
logs. Corrected = per-lane sample true-positive rate applied to raw matches, adjusted for
cross-source duplicates and residual-sweep finds.

| Repetitive action | Pixel op | Raw | TP rate | Corrected / month | Status |
|---|---|---|---|---|---|
| Commit / push / branch mutations | `ship` · `publish` · `push` | 682 | 0.54 (+0.16 partial) | ~320–430 | covered · make faster |
| Restore behavior/code from git history | `rescue` · `excavate` | 447 (+~640 in missed clusters) | 0.33 (+0.22) | ~150–250 | covered · widen triggers |
| Find things in past agent sessions | `recall` | 630 | 0.15 (+0.09) | ~90 | covered · upgrade |
| Squash / amend / force-push / organize history | — none (now `rewrite`) — | ~31 in 150-sample | — | ~60–100 | GAP → closed |
| Locate code by phrase / value / label | `resolve` · `search` | 310 | ~0.15 of real prompts | ~35–55 | covered |
| Rebase / sync branch with main | `reconcile` | 263 | 0.52 of real prompts | ~28 | covered · extend |
| Provenance: who/when/which commit introduced X | — none (now `provenance`) — | ~440 cluster ops | — | ~15–25 | GAP → closed |
| .env key surgery & restore | — none (now `env`) — | ~380 cluster ops | — | ~10–20 | GAP → closed |
| Branch inventory / "did you push everything?" | — none (now `branches`) — | ~240 cluster ops | — | ~10–20 | GAP → closed |
| Recover deleted-from-HEAD code | `excavate` | 320 | 0.03 | ~10 | covered |
| Blast radius before edits | `impact` · `changes` | 57 | 0.00 | 0 user asks | agent-side only |
| Task file-scoping | `targets` | 14 | 0.00 | 0 user asks | no demand |

## What the evidence says, per hijack

### 1 · `ship` is the reflex for "commit and push" (~320–430 asks/mo)

The single most repeated instruction in the archive is a bare mutation imperative — the
exact `commit+push` chain appears 77 times as a standalone prompt. Fifteen more per sample
are "did you commit and push?" nags, which pixel's journal answers deterministically.

> "Okay, now my theme page is absolutely perfect. Can you fucking commit it before breaking
> everything? So we have a reference." — id 1272, 2026-08-16

> "why, on this project, commit and push take so much fucking time? … several dozens of
> seconds. It's unbearable." — id 1328, 2026-08-16 — latency is itself the pain

**Mechanism:** guard escalates from nudge to deny-with-exact-substitute for
`git commit`/`git push`/`git checkout -b`; `ship` needs a benchmarked latency target (T1) —
the raw-git path being slow is a user-stated pain, so pixel must be measurably faster, not
just safer.

### 2 · `rescue`/`excavate` — right doctrine, wrong trigger vocabulary (~150–250 asks/mo)

The archive proves the doctrine's core bet: the operator repeatedly, explicitly orders
agents to retrieve from history instead of recoding — and repeatedly catches them
regenerating instead.

> "why do you create code ? this is a regression so use git history" — id 2301, 2026-08-25

> "You do not create any code… You dig into the Git history and you will find an
> implementation of my form… don't actually destroy the rest of the changes that were made."
> — id 8496, 2026-08-16 — verbatim the `rescue --apply --merge` contract

> "Try to recreate one, and it's shitty and buggy, instead of just fucking taking the one in
> the history like I asked." — id 1235, 2026-08-16

**Mechanism:** the lane regex missed ~640 rescue-shaped cluster ops because real phrasing is
voice-transcribed and oblique — "githy story", "get it back", "like it was before", "it
broke the animation", "restore please". The rule's trigger list now carries this measured
vocabulary. A regeneration detector (warn when an Edit re-creates content `excavate` can
find verbatim in history) remains open.

### 3 · `reconcile` — demand-proven, extended to the chains around it (~28 unique asks/mo)

Three separate prompts complain about exactly the stale-local-main failure reconcile
eliminates — one literally created the operator's global pre-rebase rule.

> "When I ask for rebase, for example, on main, it should automatically fetch the latest
> main, not just take the local one." — id 1073, 2026-08-14

> "Okay, so can you actually rebase that branch onto main and then continue the work?"
> — id 8878, 2026-08-16

**Mechanism:** two recurring chain shapes ended past reconcile's finish line —
rebase-then-integrate ("rebase and merge them into develop") and two-branch alignment.
`reconcile --into <target>` now covers the first: rebase the current branch onto
origin/&lt;target&gt;, fast-forward local &lt;target&gt; to the rebased head, push both, never a merge
commit. Submodules and PR-URL-addressed rebases stay out of scope (low count).

### 4 · `recall` — the most under-leveraged op (~90 asks/mo, ~3/day)

Transcript hunting bled into every other lane — it is the connective tissue of recovery.
One lost-artifact episode generated 8 prompts by itself, and the strongest unrecoverable
loss of the month (a gitignored skill file) was exactly recall's domain and excavate's
proven blind spot.

> "can you recover the user message from those session of what i couldnt send or finish"
> — id 7841, 2026-08-26

> "there was a variable that was about allowed Git user for <project> deployment. I lost the
> original file, but I'm sure it's in the transcript somewhere." — id 199, 2026-08-04

> "That's the wrong transcript you gave me, the one from another conversation." — id 7712,
> 2026-08-21 — the disambiguation failure `recall sessions` prevents

**Mechanism:** date-bounded search (asked verbatim: "less than a week ago"), new agent
stores as they appear, and bulk `recall export`. Promotion from ADVISORY to MANDATORY still
requires a T3 benchmark.

### 5 · `resolve` — right op; resolutions should persist (~35–55 asks/mo)

> "where is the code 4 digit to give to the driver ?" — id 2281, 2026-08-25

> "Find the prompt starting with \"This app is going to be\"" — id 18681, 2026-07-28 — the
> same ask re-appeared verbatim 10 days later: locates are never made durable

**Mechanism (open):** a git-anchored resolution cache (phrase → resolved location) would
make the second ask instant across sessions and agents.

## New ops — gaps with measured demand (all shipped 2026-08-31)

| # | Op | Demand evidence |
|---|---|---|
| 1 | `rewrite` — squash/amend + leased force-push | ~31/150 mutation sample: "squash it into a single commit force push" (533), "dont commit on top amend and force push" (19101), "organize commits by group and commit and push" (3234), literal `git push --force origin main develop` (20638). Every one previously forced raw git. |
| 2 | `provenance` — per-region blame attribution | ~440 cluster ops: "am i the one who introduced that? which commit" (4318), "whether any of the design was touched by me or if it was already like this", "which commits introduced those three bugs" (5480). |
| 3 | `env` — additive-only key-level .env mutations, snapshot-first, values never printed | ~380 cluster ops: "restore my .env", "add <vendor> key without deleting the rest", "you erased all the other var env restore". Untracked files have no git history — the snapshot store closes the recovery gap and mechanizes the operator's additive-only env rule. |
| 4 | `branches` — one-call inventory + deterministic "everything pushed?" | ~240 cluster ops + ~15 nags/sample: "plenty of branches without a PR. Could you clean that?", "fetch new branch someone pushed like 10 min ago". |
| 5 | `changes --tests` — affected-test selection | "can the test be incremental and run only the affected part? it's tiring to run everything" (20644) — the operator hand-built affected-test tooling elsewhere: demand proven by workaround. |
| 6 | `recall export` + `--until` parity | date bounds (68), new stores (21481), bulk export (13395), wrong-transcript complaint (7712). |

Not worth building: PR creation/merge (external service, owned by `gh` + skills), issue-tracker
sync, simulator/visual verification, live process babysitting — frequent, but none are
deterministic-retrieval-shaped. The two biggest residual clusters (stuck-orchestrator
revival and spec-compliance judgment, ~3,300 ops together) are LLM/process work, not pixel
work.

## Demoted with the same honesty (T3)

- **`targets`**: 0 of 14 lane samples were human asks — all orchestrator noise. Combined
  with the measured read-fence recall hazard (`docs/bench/sniper-discovery.md`), demand-side
  evidence rounds to zero. It stays an optional scoping aid.
- **`impact`**: 0 direct user asks in 57 samples. It survives as agent-side discipline; its
  MANDATORY status rests entirely on the still-pending A/B benchmark.

## Enforcement: warn → deny-with-substitute

"Aggressive" concretely means the guard stops nudging and starts substituting, in demand
order. Shipped as the `PIXEL_SUBSTITUTE` deny tier: plain `git commit` / `git push` /
`git checkout -b` / `git switch -c` / `git rebase` / `git commit --amend` are denied with
the exact pixel command in the message (parsed `-m`, pathspecs → repeated `--files`).
Interactive/porcelain shapes pixel cannot cover (`rebase -i`, `--continue`, `push --tags`, …)
pass through. Transcript-store pokes are denied only when a recall index actually exists —
otherwise they stay advisory (the sniper-discovery lesson). Escape hatches:
`PIXEL_GUARD_RAW_GIT=1` and `PIXEL_GUARD_RAW_TRANSCRIPTS=1` downgrade their tier to
advisory. Each blocking promotion is a T3 event: it keeps deny status only until an A/B
measurement says otherwise — that measurement is still pending and is the next bench task.

## Bounds & method (T2 — every cap named)

- **Pipeline:** 21,700 raw ops → 7,250 dedup roots → 9 regex lanes (≤150 samples each,
  biased toward short prompts and large clusters — not random) + chain mining over 6,738
  short ops + a 60-cluster residual sweep. Every lane semantically triaged by an independent
  agent; 8 quoted ops re-verified verbatim against the archive by id.
- **Known caps:** lane samples cap at 150, so TP rates for the two biggest lanes
  extrapolate from ≤24% of matches. Cross-source duplication (~10–15%, the same voice
  prompt logged 2–3×) only partially removed. Corrected counts are ranges, not points.
  The residual sweep covered the top ~200 clusters; the long tail (~2,700 small clusters)
  is unswept, so corrected counts are lower bounds for the covered shapes.
- **Noise families:** one agent system prompt duplicated 108×; one orchestrator status blob
  99×; agent-init boilerplate 75×; "transcript" as dictation output; "session" as auth/tmux.
  ~60% of the sample budget went to agent-authored noise — any re-run should filter
  agent-authored records before regex mining.
