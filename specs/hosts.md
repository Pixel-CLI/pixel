# Hosts

Status: normative. The capability matrix below is the contract `pixel-setup`'s
host table implements. A cell is **measured** (observed against a real host,
with the Pixel evidence cited), **documented** (host documentation, unverified
here) or **to verify** (checked live in M6 before the code relies on it).

## Hosts in v0.1

Claude Code, Codex, pi, OpenCode, Gemini CLI, Devin, Antigravity. Cursor,
GitHub Copilot CLI and Amp get the skill only (`skills.md`).

## One entry point

`pixel hook <event> --host <host> [--part K/N]` reads the host payload on stdin
(bounded: 1 MiB, 1 s wait for the pipe to close), runs the event, and prints
exactly one JSON envelope on stdout or nothing. It always exits 0. Every
registered command ends in ` || true` so an older binary that rejects a new
flag cannot block a prompt. An unknown event prints a diagnostic on stderr and
exits 0. A panic is caught: whatever was already printed stays, nothing is
added.

Events: `session-start`, `prompt-submit`, `pre-edit`, `post-edit`,
`post-tool`, `stop`. Per-host registration names map to them (table below).
`pre-edit` and `post-edit` apply only to edit-capable tools (`hooks.md`);
`post-tool` is the every-call fast path (feedback record, metrics relay).

## Event map and envelope shapes

| Host | prompt-submit | pre-edit | post-edit / post-tool | stop | session-start |
|---|---|---|---|---|---|
| Claude Code | `UserPromptSubmit`, `hookSpecificOutput.additionalContext` (measured: Pixel A/B) | `PreToolUse` matcher `Edit\|Write\|MultiEdit\|NotebookEdit`, `hookSpecificOutput.additionalContext` (to verify) | `PostToolUse` (matcher none for post-tool, edit matcher for post-edit), `additionalContext` (to verify), `systemMessage` for the user | `Stop`: `{"decision":"block","reason":…}` continues the turn; `stop_hook_active` set on the re-entry (documented) | `SessionStart` (`source`: startup, resume, clear, compact) |
| Codex | `UserPromptSubmit` in `$CODEX_HOME/hooks.json` (measured); needs `/hooks` review by the user | `PreToolUse` on `apply_patch` and shell tools (to verify) | `PostToolUse` (measured for the metrics relay) | `Stop` (documented; `Interrupt` also exists) | `SessionStart` |
| Devin | `UserPromptSubmit` in its config; draws no `systemMessage` or hook output, so `show_output` is set on that entry (measured) | `PreToolUse` (to verify) | `PostToolUse` | `Stop` | `SessionStart` |
| Gemini CLI | `BeforeAgent`, `hookSpecificOutput.additionalContext` (measured) | `BeforeTool` (to verify) | `AfterTool` (to verify) | `AfterAgent` (to verify) | `SessionStart` (to verify) |
| Antigravity | `PreInvocation`, `{"injectSteps":[{"ephemeralMessage":…}]}` (measured) | `PreToolUse` (to verify) | `PostToolUse` (metrics, measured), `PostInvocation` consumes pending lines | `PostInvocation` (to verify) | none |
| pi | extension event `before_agent_start`, returns `{message:{customType,display,content}}` (measured) | extension event `tool_call` (to verify) | `tool_result` (measured for bash) | `agent_end` (to verify) | `session_start`, `session_compact` (measured) |
| OpenCode | plugin `prompt` hook then `context` hook adds a system part on the turn's first request (measured) | `tool.execute.before` (to verify) | `tool.execute.after` (measured for the metrics footer) | idle event (to verify) | none |

Antigravity and Devin do not document a size cap and none was measured; they
keep one hook. Gemini documents none.

## Size limits (measured)

- **Claude Code:** `additionalContext` of one hook is capped at 10,000
  characters; 10,001 arrive as a ~2,000-character preview. Several hooks of one
  event each get 10,000 and run concurrently.
- **Codex:** a hook's context past 2,500 estimated tokens (4 bytes each) is
  spilled to a file behind a head and tail preview.
- Pixel therefore delivers a brief larger than one hook through `brief_parts`
  entries (default 3, 1 to 8): the same command with `--part K/N`. Part 1
  computes and stores the whole brief under `.pixel/brief-parts/`, parts 2..N wait
  (poll 15 ms) for the file and then for part 1's `.done` marker (the host
  orders concurrent hook output by completion time) and print their slice.
  A slice is whole lines up to 9,800 characters. Behaviour on late, missing
  and oversized parts is lifted from Pixel's `brief_parts.rs`.

## What each event may print

| Event | Model-visible | User-visible only |
|---|---|---|
| prompt-submit | `[PIXEL:BRIEF]` block | `systemMessage`: `🟩 pixel brief ❀ <bytes> ❀ <ms>` |
| pre-edit | `[PIXEL:EDIT]` card (≤1.2 KB) | `🟩 pixel edit ❀ <n> callers` |
| post-edit | `[PIXEL:STALE-CALLERS]` | `🟩 pixel stale ❀ <n> sites` |
| post-tool | nothing | metrics relay when the call ran `pixel` |
| stop | `block` reason (once per pending-set hash) | `🟩 pixel stop ❀ <n> stale sites` |
| session-start | nothing | nothing |

Where a host cannot carry context at an event (to-verify cells that fail), the
card moves to the next event that can, and the spec records the fallback here.
Devin and Antigravity have no user-only channel: they get the metrics line as a
model-visible request to repeat it once, as the previous Pixel engine did.

## pi package

`~/.local/share/pixel/pi-package/` with `package.json` and `extensions/`,
declared in the `packages` list of pi's `settings.json` (every other key kept).
Binary path baked at install. Extensions:

- `pixel.ts`: `session_start`/`session_compact` → `pixel hook session-start --host pi`;
  `before_agent_start` → brief as an `pixel-brief` custom message followed by the
  untrusted-data trailer, asked for the text after the `<skill …>…</skill>` block
  pi expands `/skill:name <text>` into (none for a bare `/skill:name`) and drawn
  by the extension's renderer as one summary line that a left click or
  `app.tools.expand` (ctrl+o) opens; `tool_call`/`tool_result` on edit tools →
  `[PIXEL:EDIT]` / `[PIXEL:STALE-CALLERS]` messages with `triggerTurn: false`;
  `tool_result` of a bash call that ran `pixel` → metrics relay via
  `ctx.ui.notify`; `agent_end` → stop check, sent with `triggerTurn: true` so
  the agent continues. It never blocks a tool and drops every failure.
- `pixel-impact.ts`: `/pixel-impact <symbol>` runs `pixel impact <symbol> --no-refresh
  --depth 2 --json --metrics off` and shows ≤12,000 bytes without starting a turn.
- `pixel-classify-files.ts` (second package `pi-classify/`, only when a classify
  engine is accepted): five tools over `pixel classify` (`ask_pixel_file_bool`,
  `ask_pixel_file_choice`, `ask_pixel_file_score`, `ask_pixel_files`, `pick_pixel_file`).

pi's hook subprocess deadline is shorter than the native hosts': the brief
keeps 6 s there (8 s elsewhere) and the extension kills the child at 6.5 s.

## Verification obligations (M6)

For every to-verify cell: run the host, capture a real payload and the effect
of an envelope, and update this table from "to verify" to "measured" with the
date and host version, or record the fallback. Pi's event names and the
`triggerTurn` continue semantics are verified against the installed
`@earendil-works/pi-coding-agent`. Hosts not installed in the build container
are verified by the maintainer with the procedure below.

### Procedure (maintainer, per host)

```sh
pixel setup --agent <claude|codex|devin|gemini|antigravity|opencode|pi> --yes
scripts/verify/capture.sh on            # wraps the installed pixel; hooks now leave their payloads
# In the host, in a scratch repository: send a prompt that names a symbol,
# have the agent edit a file that has callers, let it finish the turn.
scripts/verify/capture.sh show          # event, stdin payload, stdout envelope, exit code
scripts/verify/capture.sh off
```

For each "to verify" cell of the host, the capture must show: the event name
the host sent (`hook_event_name` or its equivalent), the payload fields
`pixel-setup` reads (`Payload`), and, for an envelope, the effect in the host
(the model's next message cites the injected text; the 🟩 line is drawn).
Report the host version and any cell whose behaviour differs, with the
capture; the cell is then changed to "measured" (date, version) or the
fallback is recorded. Codex also needs `/hooks` to review the entries.
`pixel doctor --only 'install.*'` after `pixel setup` confirms the registration.
