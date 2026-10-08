---
name: arena
description: Run a live pi agent in a fresh Herdr pane and exercise one of eight standard prompt types, waiting for an idle result and recording local run metadata.
---

# arena live run

This skill is a small live-agent arena. Each run uses a fresh scratch
workspace and one prompt from the eight-case catalog below. Never kill or
close anything that is not an earlier `arena*` test pane. On restart, close
only stale `arena*` panes, then open a fresh one. The pane created by this
procedure is the only pane this skill owns.

## Prompt catalog

Run one case at a time and record its `PROMPT_TYPE` in the local receipt.
Keep the prompt's contract intact when adapting the target filename to the
scratch workspace.

1. `lookup` — recover an exact identifier, constant, signature, or initializer.
   `What is the maximum rendered byte budget for the prompt-submitted execution brief, and what operation/time limits enforce it?`
2. `impact` — find a definition's production callers and the tests that exercise it.
   `Where is start_brief defined, which production path calls it, and which named tests cover that path?`
3. `flow` — trace a behavior across registration, events, processing, and output.
   `Trace prompt-submit from registration through task-event processing to the host context envelope.`
4. `diagnosis` — explain a failure, fallback, or stale-state behavior from source and tests.
   `Why can a stale graph still leave text evidence available, and what tests prove the fallback behavior?`
5. `configuration` — identify the exact setting, command, event mapping, or doctor rule.
   `Which command and event mapping configure the Codex prompt-submit hook, and what does doctor validate?`
6. `tests` — identify the relevant test cases and the contract assertions they make.
   `Which tests cover Pi exclusion, provider routing, and the execution-brief envelope, and what does each assert?`
7. `history` — recover constraints, intent, or prior behavior from recorded commits.
   `What constraints did the commit that introduced the execution brief record, and which files changed with it?`
8. `architecture` — explain ownership boundaries and source handoffs between modules.
   `Which modules own retrieval, rendering, and host context, and where are the handoffs between them?`

## 1. Close the previous test pane, then create a fresh workspace and pane

```bash
herdr agent list | jq -r '.result.agents[] | select(.name // "" | startswith("arena")) | .pane_id' | while read p; do herdr pane close "$p"; done
ROOT=$(mktemp -d /tmp/pi-herdr-arena-XXXX)
mkdir -p "$ROOT/ws"
PANE=$(herdr pane split --direction right --cwd "$ROOT/ws" | jq -r '.result.pane_id // .pane_id')
```

Never run the test agent inside this repository: its writes land in
`$ROOT/ws`, where they can be inspected safely.

## 2. Start pi

Choose a unique `arena<N>` name and set the prompt metadata before starting:

```bash
RUN=arena<N>
PROMPT_TYPE=lookup
PROMPT='What is the maximum rendered byte budget for the prompt-submitted execution brief, and what operation/time limits enforce it?'
STARTED_AT=$(date -u +%Y-%m-%dT%H:%M:%SZ)
herdr agent start "$RUN" --kind pi --pane "$PANE" --timeout 60000 -- --no-session
```

To test working-tree extension changes, pass every extension file explicitly;
`--extension` does not accept a directory:

```bash
cd /Users/livio/Documents/pi-ultimate
herdr agent start "$RUN" --kind pi --pane "$PANE" --timeout 60000 -- --no-session \
  $(for f in extensions/*.ts extensions/*/index.ts; do printf -- "--extension %q " "$PWD/$f"; done)
```

## 3. Send the selected prompt

```bash
herdr agent prompt "$RUN" "$PROMPT"
```

A timeout or a “continues in background” reply means the prompt was
submitted; do not resend it.

## 4. Wait for idle, then record the local receipt

```bash
herdr agent wait "$RUN" --until idle --timeout 120000
ENDED_AT=$(date -u +%Y-%m-%dT%H:%M:%SZ)
STATS_FILE="${ARENA_STATS_FILE:-$ROOT/arena-runs.jsonl}"
mkdir -p "$(dirname "$STATS_FILE")"
jq -nc \
  --arg run "$RUN" --arg type "$PROMPT_TYPE" --arg prompt "$PROMPT" \
  --arg root "$ROOT" --arg started "$STARTED_AT" --arg ended "$ENDED_AT" \
  --arg revision "$(git rev-parse HEAD 2>/dev/null || true)" \
  --arg skill_sha "$(shasum -a 256 "$PWD/.agents/skills/arena/SKILL.md" | awk '{print $1}')" \
  '{run_id:$run,prompt_type:$type,prompt:$prompt,workspace:$root,started_at:$started,ended_at:$ended,status:"idle",revision:$revision,skill_sha256:$skill_sha,scenario_id:$type,artifacts_path:($root + "/ws")}' \
  >> "$STATS_FILE"
```

The default JSONL receipt is local to the scratch run. Set
`ARENA_STATS_FILE="$PWD/.pixel/arena-runs.jsonl"` (or another private path)
when you want durable history. Do not store credentials, full transcripts, or
machine-specific secrets. Useful durable fields are run id, prompt type,
scenario hash, start/end timestamps, status, changed-file count, test outcome,
input/output/cache/total tokens, tool and native-command counts, cache
category, artifact hashes, and the exact skill/revision used. For a full Arena
experiment, keep raw JSONL transcripts in a gitignored
`eval/arena-results/<run-id>/` directory and put only a compact redacted
receipt under version control.

## 5. Leave it running

Leave the current `arena<N>` alive for inspection or more prompts. The next
arena run closes stale `arena*` panes before creating a fresh one.
