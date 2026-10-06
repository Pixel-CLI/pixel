---
name: pixel-classify
description: Make fast bounded classification decisions through `pixel classify` — labels plus per-label criteria in, a calibrated probability distribution and confidence out. Use when a task or a harness needs a typed judgment — intent routing, risk scoring, yes/no gates, triage, severity grading — without writing Jev integration code. Also use when building or improving an agent harness that should route, gate or grade on cheap model verdicts.
---

# pixel-classify — bounded decisions via `pixel classify`

## Purpose

`pixel classify` is a System One-style decision call exposed as a CLI: one
state text, a shared context (the question and rubric), and a bounded label
set with per-label criteria go in; a probability distribution over the
labels plus a confidence come out. It answers in well under a second on the
local engine and costs almost nothing.

Reach for it whenever a judgment call would otherwise cost a long think: is
this prompt a bugfix or a feature, is this command safe to run unattended,
which file should be opened first, how risky is this diff. The model picks
a card from your deck — it cannot invent an option — so every answer maps
straight onto an action.

## Prerequisites

- `pixel` on PATH (`command -v pixel`).
- Classify enabled once: `pixel config classify on`. If a call returns
  `classify is disabled`, run it.
- Engine: `pixel config classify-engine` — `local`, `remote`, `jev`
  (TypeSafe's hosted decision model), or `auto` (default) probes the
  local Ollaya server (`http://127.0.0.1:11435`, TypeSafe-compatible
  `/v1/systemone`) and falls back to a remote preset. A remote preset's
  key resolves from (first hit wins): its own environment variable
  (`TYPESAFE_API_KEY` for jev, `OPENROUTER_API_KEY`, `OPENCODE_API_KEY`,
  or the override named by `PIXEL_REMOTE_KEY_ENV`), a key stored via
  `pixel config remote-key <preset> <key>`, or a configured Infisical
  project (`INFISICAL_TOKEN` + `PIXEL_INFISICAL_PROJECT_ID`).
- `pixel classify --if-warm` answers only from an already-listening local
  engine and fails otherwise — use it when the call must not hit the
  network.

## The call shape

```bash
pixel classify '<STATE>' \
  --context '<the question and rubric every label shares>' \
  --label <a> --label <b> --label <c> \
  --criterion <a>='<observable situation for a>' \
  --criterion <b>='<observable situation for b>' \
  --json
```

- `STATE` (positional): the part that varies — the prompt, the command, the
  diff summary. Keep it short; isolate what is judged.
- `--context`: the shared framing — the question plus the rubric preamble.
  It is replicated into each candidate, not added to the state.
- `--label`: candidate labels, repeatable or comma-separated.
- `--criterion label="..."`: one described situation per label.
- Output JSON: `predicted`, `probs` (distribution over labels),
  `snapshot.confidence`, `snapshot.model`, `snapshot.provider`.

Other modes:

- `pixel classify --task-intent '<prompt>'` — built-in coding-agent intent
  labels; the verdict names the pixel ops that fit.
- `pixel classify --debug` — asks every configured engine (local Ollaya,
  the remote chat preset, and Jev) the same labeled question in parallel
  and prints each answer or per-engine error; the comparison view for
  checking whether the engines agree before you trust one. `--remote-model`
  and `PIXEL_REMOTE_*` reach only the selected preset's lane: beside
  another preset, the Jev lane uses Jev's own key, base and model.
- `pixel classify --jsonl` — serve mode: one JSON spec per stdin line, one
  result per line. Use for batches instead of a shell loop.

## Mapping the three decision shapes

| Shape | pixel classify form | Read |
|---|---|---|
| yes/no probability | `--label yes --label no` with criteria for each | `probs.yes` is the probability, 0–1; branch on your threshold |
| pick one of ≤255 | N `--label`s + `--criterion` each | `predicted` + `probs` + `snapshot.confidence` |
| score (2–10 ordered levels) | ordered `--label`s, low → high | distribution over levels; `predicted` is the nearest level |

## Question design

1. **One snap judgment per call.** "Does this convey urgency?" — not
   "analyze and decide the best action."
2. **Describe situations, not degrees.** `"Blocking issue; no workaround
   exists"` beats `"moderately severe"`.
3. **Give the model an exit.** Add an `other` / `none_of_the_above` label
   whenever your list might not cover every input.
4. **Confidence is a second axis.** `predicted` says what,
   `snapshot.confidence` says whether. Set a floor (below → route to a
   human/ask), a bar (above → auto-execute), and treat the middle as
   confirm-first.
5. **Isolate the state.** Pass the line being judged, not the whole file.
   Read files yourself only when you need the code, not the verdict.
6. **Numbers, dates, and counting stay in code.** The model picks a card
   from the deck; it does not name one. Filter candidates first, then
   classify.
7. **Bounded label set is the feature.** The answer is always one of your
   labels — map each to an allowed action before calling.
8. **It does not write.** Use it for gates, flags, routing, and grading —
   not for generating text, exact lookups, counting, or anything
   `pixel search-content` answers deterministically. It is also not a
   long-running agent: no control loops, no UI operation, no open-ended
   execution — one bounded snap judgment per call.

## Wiring it into a harness

Classify is cheap enough to sit on a harness's decision boundaries:

- **Routing**: classify each incoming prompt (`--task-intent` or your own
  labels) and pick the agent, model tier, or tool set before work starts.
- **Guards**: gate tool calls in a `PreToolUse`-style hook — classify the
  command or file path as `safe` / `review` / `block` before allowing it.
- **File triage without context burn**: when pi is the harness, the
  `pixel-classify-files` extension (`pixel install` offers it once classify
  is configured) registers `ask_pixel_file_bool/choice/score`,
  `ask_pixel_files`, and `pick_pixel_file` — the file's text never enters
  the agent's context, only typed verdicts do. If those tools are present
  in the session, prefer them over reading a file just to judge it —
  pi does not surface them on its own.
- **Grading**: score diffs, plans, or generated patches on an ordered
  rubric before auto-applying or handing to review.

For a production TypeScript integration of the underlying decision API
(Jev, TypeSafe's System One model), embed the `/v1/systemone` contract in
shipped code rather than shelling out per call.

## Examples

Gate a command before running it unattended:

```bash
pixel classify 'rm -rf node_modules && bun install' \
  --context 'Is this shell command safe to run unattended?' \
  --label safe --label review --label block \
  --criterion safe='read-only or fully reversible' \
  --criterion review='destructive but local and recoverable' \
  --criterion block='irreversible or touches credentials or production' \
  --json
# → predicted: "review", probs {safe .06, review .80, block .15}, confidence .70
```

Triage a user request:

```bash
pixel classify 'the export button silently does nothing after the update' \
  --context 'What kind of work does this request describe?' \
  --label bugfix --label feature --label question --label other \
  --json
```

Score a diff before committing — paste a short summary of what changed, not
the whole diff:

```bash
pixel classify 'touches auth token refresh path, no tests changed' \
  --context 'How risky is this change to ship?' \
  --label low --label medium --label high \
  --criterion low='isolated, tested, no shared callers' \
  --criterion medium='some callers touched or partial coverage' \
  --criterion high='security-sensitive or untested shared path' \
  --json
```

## Failure handling

- Exit non-zero with `classify is disabled` → `pixel config classify on`.
- `--if-warm` prints nothing and fails when no local engine listens —
  retry without it or check the Ollaya server.
- Remote engine is network-bound and non-deterministic; do not treat
  identical calls as guaranteed identical answers.
- Low `snapshot.confidence` is a valid answer: it means escalate the
  decision, not retry until the number moves.

## Report format

When you act on a verdict, report the label, its probability, and the
confidence — e.g. `classified: block (p=0.81, conf=0.74) → refusing to
run`. Never present a `pixel classify` verdict as deterministic fact.
