# Thesis

Status: normative. Every other spec answers to this one.

## What Pixel is

Pixel speeds up coding-agent work by using deterministic mechanisms for steps
the model should not have to repeat. Its local engine runs inside agent hooks
and injects evidence that avoids repeated investigation. Retrieval,
the graph and the prompt-submit brief form the core; token savings follow from
less repeated work.

This engine was developed as RTO (Rust Token Obliteration), a clean rewrite of
the previous Pixel engine pinned at `e8fd304`, and now keeps the Pixel product
name. That engine's measured value was the prompt-submit brief that removed
turns. Its historical CLI and API are not compatibility requirements for this
rewrite; Git and browser workflows belong in separate plugins.

## Evidence it rests on (Pixel 0.9.0, 13 ripgrep questions, n=3)

| Agent | Tool calls | Provider-reported tokens | Blind answer grade (/3) |
|---|---|---|---|
| Claude Code | 149 → 59 | −43.3% | 2.95 → 2.85 |
| Codex | 210 → 32 | −78.3% | 3.00 → 2.95 |

RTK (output trimming) simulated on the same sessions saved 9.0% / 4.7%.
Optional capabilities offered to the model are not used: with xerj installed
as MCP tools plus a skill the agent made 58 calls and none to xerj; skill-forced
Pixel chains cost 36–95% more tokens than native retrieval. Mid-session relays
scored below baseline in Pixel's Sept 2026 evaluation. These numbers are the
baseline the rewrite must meet (`roadmap.md`, acceptance). These are historical
Pixel 0.9.0 results, not measurements of this candidate; comparative benchmarks
have not yet run.

## Principles

1. **The value is less repeated agent work.** Measure tool calls, elapsed time,
   tokens and answer correctness. Removing a model turn is one way to improve
   that work; retrieval is a means, never the product.
2. **Push, don't offer.** A capability that matters runs inside a hook at the
   moment it changes the agent's next action, and its *result* is injected. The
   model is never asked whether to call it. A skill exists only as a fallback.
3. **Deterministic.** Same repository state, same prompt, same bytes. No model
   in the default path. Classification and semantic search are opt-in.
4. **Quiet unless actionable.** After the brief, a mid-session injection fires
   only when deterministic evidence says the agent's next action is wrong or
   incomplete. Silence is the default and is measured.
5. **Fail open.** A hook never blocks, delays past its budget, or fails a host.
   Every error exits 0 with a stderr diagnostic.
6. **Every claim is metered locally.** Each brief, card and block leaves a
   record; nothing leaves the machine (`metrics.md`).
7. **Complementary to RTK.** Pixel avoids repeated investigation. It never filters test, build or
   diff output.
8. **Honest about coverage.** An answer states what it could not see
   (`lower_bound`, `Likely` vs `Proved`). "No callers found" never means "none
   exist".

## Non-goals

- No MCP server, no hosted service, no account, no telemetry.
- No search interception: Pixel never denies `grep` or a file read.
- No model call in the default path.
- No git mutation commands in core (they live in the separate git plugin).
- No task contracts, verification receipts or edit gating.

## Success criteria for v0.1.0

- Arena evidence (`roadmap.md`): compare pinned Claude Code and Codex trials
  using tool calls, tokens, elapsed time and answer quality. Historical Pixel
  results are reference measurements; a release acceptance threshold still
  needs an explicit maintainer decision.
- Every host in `hosts.md` installs, relays and uninstalls byte-exact.
- Edit hooks are shipped off by default and turn on only on a measured gain.
