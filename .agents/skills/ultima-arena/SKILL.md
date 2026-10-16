---
name: ultima-arena
description: "Install Pixel from a repository, start Claude Code, Codex, pi, agy and Devin in one Herdr grid in random order, send each the same neutral prompt, and judge per CLI from evidence only whether the prompt-submit brief and a Pixel metrics line reached it. Use for /ultima-arena, 'ultima arena', 'test brief and metrics across all CLIs', or after a change to a hook, relay or install."
---

# ultima-arena: brief and metrics, five CLIs, evidence only

`ultima_arena.py` (next to this file) answers two questions for each CLI,
from files and screens, never from what the model says:

| Check | PASS when |
| --- | --- |
| `brief` | **logged**: `<repo>/.pixel/brief-feedback.jsonl` holds a `prompt` line and a `part` line with `bytes > 0` of that CLI's `provider`, both with `ts` at or after the moment the prompt was sent; **and on screen**: the pane shows the brief's text (`[PIXEL:BRIEF]`, pi's `[pixel-brief]`) or the `🟩 pixel prompt-submit ❀ Xms` notice, which the hook adds only to a delivered brief, or, for Devin (which draws neither), its `✱ Hook completed: … --provider devin --event prompt-submit` line, printed because `pixel setup` sets `show_output` on that hook. |
| `metrics` | the pane (for agy, its store; see below) shows `🟩 pixel search-content … #<id>` and `<repo>/.pixel/actions.jsonl` holds a `search-content` record, `ts_ms` at or after the prompt, whose `invocation_id` middle field ends in that `<id>` (last six characters, as `format_metrics_line` shows). |

**agy draws no hook output.** agy 1.4.0 shows nothing a hook returns: of every response
shape tried, only an injected `userMessage` step appears, and it appears as a fake user turn.
`verbosity: high` adds only thinking lines. So for agy the arena reads, beside the screen,
what agy stored: `~/.gemini/antigravity-cli/conversations/<conversationId>.db`, where the
conversation id is the `session` of the turn's `prompt` line (or, when no line names one, every
file written since the prompt). Only steps of type 90 count: each one is an injected
`ephemeralMessage`. The tool's own output is type 132, so its box never passes for the relay.
`[PIXEL:BRIEF]` there is the brief (`screen_evidence: "agy store: [PIXEL:BRIEF]"`), and a 🟩
box there is the relay. The receipt lists the conversations read (`agy_store`), and
`agy.injected.txt` holds the steps. This proves the brief and the relay reached agy's model;
agy's user sees neither.

`NO-CALL` (metrics) means the model answered without running the Pixel
command: there was nothing to relay, so it is neither a pass nor a relay
failure, and the run still exits 1. The prompt asks for the command before the
question for that reason.

The prompt never mentions metrics (a hard guard refuses `metric`, `🟩`, `❀`)
and the answer is never scored: a model asked about metrics says yes.

```bash
S=.agents/skills/ultima-arena/ultima_arena.py
python3 $S --repo ~/Documents/pixel               # install, then run (from a Herdr pane)
python3 $S --repo ~/Documents/pixel --skip-install  # use the pixel already on PATH
python3 $S abort                                   # close the panes the last run created
python3 $S --self-test                             # offline contract, no Herdr, no model
```

Flags: `--repo PATH` (default: git toplevel of the cwd; may be another checkout
than the script's), `--skip-install`, `--seed N` (default time-based, printed
and stored), `--clis claude,codex,pi,agy,devin`, `--timeout SECS` per CLI
(default 300), `--out DIR` (default `<repo>/eval/arena-results/ultima-<UTC>/`,
gitignored). Exit 0 when every CLI passes both checks, 1 otherwise, 2 for a
usage or environment problem (no Herdr pane, `PIXEL_BRIEF_LOG` off, install failed).

## What a run does

1. **Install** (unless `--skip-install`): `pixel self-update --repo <repo> --build "cargo build --profile dev-release -p pixel-cli"`,
   `pixel setup --yes`, `pixel build-index --history .`, `pixel doctor <repo> --fail-on yellow --json`.
   Self-update and setup are fatal (a mise or Homebrew `pixel` is refused by
   self-update itself: the message is surfaced); the index and a yellow doctor are recorded, not fatal.
   The receipt records `pixel --version`, `command -v pixel` and the repo HEAD. It rewires the **home** install
   to this build, as AGENTS.md "Optional Local Install Verification" describes; hand it back with `pixel setup` afterwards.
2. **Grid** in the operator's tab (`$HERDR_PANE_ID`, which stays on the left with 30 % of the width): five cells on the right, three on top and two below,
   split sequentially (parallel Herdr splits race) with ratios 1/3, 1/2 for equal columns. Panes and agents are named `ultima-<cli>`;
   every created pane id is saved to `$TMPDIR/ultima-arena-<uid>/last-run.json` as it is made.
3. **Order**: `random.Random(seed).shuffle`; agents are started and prompted in that order. An `agent_pane_busy` start is retried.
4. **Dialogs**: Codex's hook review (`hooks need review`, `t trust all`) is trusted only when every hook command in
   `$CODEX_HOME/hooks.json` and `<repo>/.codex/hooks.json` is Pixel's own `<path>/pixel run-hook …`; otherwise the CLI is
   `needs-operator` and no key is pressed. A folder-trust dialog is accepted (option number, then Enter); a tool approval is
   accepted only when the pending command is exactly the arena's `pixel search-content` line. Anything else ends as `blocked-dialog` with the screen excerpt.
5. **One turn at a time**: all five start, but the next CLI is prompted only after the previous turn ended (plus a 3 s gap).
   The metrics relay stays silent when Pixel invocations overlap in one repository (a run that prompted on the previous
   CLI's first record lost Claude's relay that way), and an exclusive window ties every `actions.jsonl` record to one CLI:
   the Pixel shell call is counted from the log, not from the screen, which wraps or hides it. Run no other Pixel command in `--repo` during the run.
6. **Wait and judge**: `herdr agent prompt` without `--wait` (it reports `agent_prompt_stalled` for a prompt that ran), then poll
   `agent get` until the turn ended: `done` (Claude, Codex, agy, Devin) or `idle` (pi) after the state counter or the screen
   moved, then three polls of a still screen (Herdr sees no state change on pi). 🟩 headers are also matched with line breaks
   joined, since a narrow pane hard-wraps them. Scrollback is read with `agent read --source recent-unwrapped --lines 2000` because pi's notices scroll out of `visible`.
7. **Receipt** in `--out`: `receipt.json` (identity, seed, order, prompt, per CLI the evidence and verdicts, install steps),
   `receipt.md` (table), `<cli>.scrollback.txt`. The last line printed is `N/5 brief, M/5 metrics`.

Statuses: `done`, `timeout`, `start-failed`, `blocked-dialog`, `needs-operator`. Per CLI the receipt also holds the
`prompt-submit` relay line (Claude, Codex, pi), the shell call count, and for metrics `seen`, `occurrences` and
`outside_tool_framing` (a box not inside `│ ⎿ └` framing; best effort, a heuristic on the screen text).

## What it proves, and what it does not

- Proves: for this repo, binary and prompt, the hook wrote a brief that the host received (`part` bytes) and the user's screen showed a
  metrics box that belongs to a real invocation of that CLI's turn.
- Does not prove the model used the brief, that a box is the *relay* rather than the shell tool's own output (look at `occurrences` and `outside_tool_framing`),
  or anything about prompts other than this one. One prompt, one run per CLI: a FAIL is a lead to confirm with `agent-session-debugging`, not a verdict on a flaky run.
- `PIXEL_BRIEF_LOG=0|false|off` removes the brief evidence: the run refuses to start.
- The prompt's search path `crates/pixel/src` must exist in `--repo`, so it runs against a pixel checkout.

## Provider strings (crates/pixel/src/task_hook.rs `TaskProvider::as_str`)

`claude`, `codex`, `pi`, `antigravity` (the `agy` CLI), `devin`. Timestamps: `ts` in milliseconds on every `brief-feedback.jsonl` line, `ts_ms` on `actions.jsonl`.
