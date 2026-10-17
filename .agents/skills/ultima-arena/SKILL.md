---
name: ultima-arena
description: "Install Pixel from a repository, start Claude Code, Codex, pi, agy and Devin in one Herdr grid in random order, send each the same neutral prompt, and judge per CLI from evidence only whether the prompt-submit brief and a Pixel metrics line reached it. Use for /ultima-arena, 'ultima arena', 'test brief and metrics across all CLIs', or after a change to a hook, relay or install."
---

# ultima-arena: brief and metrics, five CLIs, evidence only

`ultima_arena.py` (next to this file) answers two questions for each CLI,
from files and screens, never from what the model says:

| Check | PASS when |
| --- | --- |
| `brief` | **logged**: `<repo>/.pixel/brief-feedback.jsonl` holds a `prompt` line and a `part` line with `bytes > 0` of that CLI's `provider`, both after the prompt; **and delivered**: the pane shows brief text or the prompt-submit notice, Devin shows its inspected hook-completed line, Pi persists its `pixel-brief` custom message, or agy stores the injected brief (below). |
| `metrics` | the pane, Pi's paired bash tool result, or agy's injected store step contains `🟩 pixel search-content … #<id>`, and `<repo>/.pixel/actions.jsonl` holds a `search-content` record after the prompt with that invocation ID. Pi also checks the exact command and repository. |

**agy draws no hook output.** agy 1.4.0 shows nothing a hook returns: of every response
shape tried, only an injected `userMessage` step appears, and it appears as a fake user turn.
`verbosity: high` adds only thinking lines. So for agy the arena reads, beside the screen,
what agy stored: `~/.gemini/antigravity-cli/conversations/<conversationId>.db`, where the
conversation id is the `session` of the turn's `prompt` line (or, when no line names one, every
file written since the prompt). Only steps of type 90 count: each one is an injected
`ephemeralMessage`. The tool's own output is type 132, so its box never passes for the relay.
`[PIXEL:BRIEF]` there is the brief (`delivery_evidence: "agy store: [PIXEL:BRIEF]"`), and a 🟩
box there is the relay. The receipt lists the conversations read (`agy_store`), and
`agy.injected.txt` holds the steps. This proves the brief and the relay reached agy's model;
agy's user sees neither.

`NO-CALL` (metrics) means the model answered without running the Pixel
command: there was nothing to relay, so it is neither a pass nor a relay
failure, and the run still exits 1. The prompt asks for the command before the
question for that reason.

The prompt requires the exact Pixel shell command even when the brief or
prior context already contains the answer; it says to report `NOT RUN` if the
command cannot run. This prevents a correct answer from hiding a missing tool
call. It never mentions metrics (a hard guard refuses `metric`, `🟩`, `❀`),
and the answer is never scored: a model asked about metrics says yes.

Run the repo workflow from a Herdr operator pane. `just` is optional; the
script works directly too:

```bash
just ultima-arena                              # fixed seed 42, install, then run the five-CLI pane grid
just ultima-arena --skip-install               # same fixed seed, use the Pixel already on PATH
just ultima-arena --seed 7                     # choose and record another fixed start order
just ultima-arena abort                       # close only panes created by the last run
just ultima-arena --self-test                 # offline contract, no Herdr, no model
scripts/dev/ultima-arena.sh --seed 42          # same runner without just
```

The default seed 42 fixes agent start order and is recorded in the receipt;
model outputs remain live. The runner submits the prompt to every ready CLI
before monitoring any turn. It then monitors all panes and interrupts each CLI
after its submitted brief and matching metrics relay are evidenced.
`scripts/dev/ultima-arena.sh` resolves this
checkout and forwards all harness options. For a live run, it requires
`HERDR_PANE_ID` from the operator pane so the grid is created beside that pane.

Flags: `--repo PATH` (default: git toplevel of the cwd; may be another checkout
than the script's), `--skip-install`, `--seed N` (default 42, printed
and stored), `--clis claude,codex,pi,agy,devin`, `--timeout SECS` per CLI
(default 300), `--out DIR` (default `<repo>/eval/arena-results/ultima-<UTC>/`,
gitignored). Exit 0 when every CLI passes both checks, 1 otherwise, 2 for a
usage or environment problem (no Herdr pane, `PIXEL_BRIEF_LOG` off, install failed).

## What a run does

1. **Overlap warning and install**: before any install or pane changes, `herdr agent list` is checked for working agents in the same repo/worktree family as `<repo>`; sibling Git worktrees count through their shared Git directory. The warning does not stop the run. Unless `--skip-install`, the arena runs `pixel self-update --repo <repo> --build "cargo build --profile dev-release -p pixel-cli"`,
   `pixel setup --yes`, `pixel build-index --history .`, `pixel doctor <repo> --fail-on yellow --json`.
   Self-update and setup are fatal (a mise or Homebrew `pixel` is refused by
   self-update itself: the message is surfaced); the index and a yellow doctor are recorded, not fatal.
   The receipt records `pixel --version`, `command -v pixel` and the repo HEAD. It rewires the **home** install
   to this build, as AGENTS.md "Optional Local Install Verification" describes; hand it back with `pixel setup` afterwards.
2. **Grid** in the operator's tab (`$HERDR_PANE_ID`, which stays on the left with 30 % of the width): five cells on the right, three on top and two below,
   split sequentially (parallel Herdr splits race) with ratios 1/3, 1/2 for equal columns. Panes and agents are named `ultima-<cli>`;
   every created pane id is saved to `$TMPDIR/ultima-arena-<uid>/last-run.json` as it is made.
3. **Order**: `random.Random(seed).shuffle`; agents are started and prompts are dispatched in that order, with no wait between turns. An `agent_pane_busy` start is retried.
4. **Dialogs**: agy's one-time `Verbosity · esc to dismiss` banner is dismissed with Esc. Codex's hook review (`hooks need review`, `t trust all`) is trusted only when every hook command in
   `$CODEX_HOME/hooks.json` and `<repo>/.codex/hooks.json` is Pixel's own `<path>/pixel run-hook …`; otherwise the CLI is
   `needs-operator` and no key is pressed. A folder-trust dialog is accepted (option number, then Enter); a tool approval is
   accepted only when the pending command is exactly the arena's `pixel search-content` line. Anything else ends as `blocked-dialog` with the screen excerpt.
5. **Dispatch together, then monitor**: prompts are sent to every ready CLI before waiting on any response. Pixel metrics may be suppressed when invocations overlap in one repository. Each relay is attributed only when that pane displays its invocation ID and the shared `actions.jsonl` contains the matching record; an overlapping timestamp alone is not attribution. `NO-CALL` uses that pane's displayed search command (or its matched relay evidence), never another CLI's action-log record. Run no other Pixel command in `--repo` during the run.
6. **Wait and judge**: Pi uses its native fullscreen mode and a fresh run-owned `--session-dir`. Its persisted `pixel-brief` custom message proves injection; an exact bash tool call paired by ID with its tool result proves the command. A metrics box in that result must match the repo's action record. The receipt retains transcript paths, provider and model; assistant prose is never evidence. This also works when Pi's terminal viewport is scrolled to an older message. Pi completion comes from its terminal assistant event, so another CLI's action cannot finish Pi's turn. Claude starts with `--verbose`; when Codex finishes with hidden evidence, the runner opens its detailed transcript once with Ctrl+T. Neither step sends another model prompt.
   `herdr agent prompt` runs without `--wait` (it reports `agent_prompt_stalled` for a prompt that ran), then poll
   `agent get` until the turn ended: `done` (Claude, Codex, agy, Devin) or `idle` (pi) after the state counter or the screen
   moved, then three polls of a still screen; Pi additionally needs its persisted completion event. 🟩 headers are also matched with line breaks
   joined, since a narrow pane hard-wraps them. Final scrollback is read from the exact pane with `pane read --source recent-unwrapped --lines 2000`; agent-level output may contain only the final answer. Wrapped `prompt-submit` and `search-content` tokens are normalized before judging.
7. **Receipt** in `--out`: `receipt.json` (identity, run number, seed, order, prompt, per CLI the evidence and verdicts, install steps),
   `receipt.md` (dated table), `<cli>.scrollback.txt`, and timestamped `run.log`. `eval/arena-results/README.md` indexes each dated run with its seed, order, counts, repository commit and dirty state; `eval/.gitignore` keeps these artifacts out of Git. The last line printed is `N/5 brief, M/5 metrics`. A process lock refuses overlapping arena runs before pane changes or installation; default output paths include microseconds so a repeat never overwrites a previous receipt.

Statuses: `done`, `evidence-complete` (turn interrupted after both signals), `timeout`, `start-failed`, `blocked-dialog`, `needs-operator`. Per CLI the receipt also holds the
`prompt-submit` relay line (Claude, Codex, pi), the shell call count, and for metrics `seen`, `occurrences` and
`outside_tool_framing` (a box not inside `│ ⎿ └` framing; best effort, a heuristic on the screen text).

Cleanup records each created pane's terminal identity. A Pi renderer crash may
remove its agent binding and arena label; cleanup closes that shell only when
the recorded terminal identity and specific renderer crash signature match,
and preserves a pane assigned to another agent.

## What it proves, and what it does not

- Proves: for this repo, binary and prompt, the hook wrote a brief that the host received (`part` bytes plus displayed or persisted injection evidence), and the screen or host tool-result record contains a metrics box belonging to a real invocation of that CLI's turn. The receipt labels the evidence source; persisted context is not reported as on-screen visibility.
- Does not prove the model used the brief, that a box is the *relay* rather than the shell tool's own output (look at `occurrences` and `outside_tool_framing`),
  or anything about prompts other than this one. One prompt, one run per CLI: a FAIL is a lead to confirm with `agent-session-debugging`, not a verdict on a flaky run.
- `PIXEL_BRIEF_LOG=0|false|off` removes the brief evidence: the run refuses to start.
- The prompt's search path `crates/pixel/src` must exist in `--repo`, so it runs against a pixel checkout.

## Provider strings (crates/pixel/src/task_hook.rs `TaskProvider::as_str`)

`claude`, `codex`, `pi`, `antigravity` (the `agy` CLI), `devin`. Timestamps: `ts` in milliseconds on every `brief-feedback.jsonl` line, `ts_ms` on `actions.jsonl`.
