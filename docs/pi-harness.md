# Pixel in the Pi harness

Pi keeps its native tools. Pixel adds one explicit command, `/pixel-impact
<symbol>`, a `before_agent_start` hook that adds the `[PIXEL:BRIEF]` evidence
brief to each code prompt, a relay that shows Pixel's `🟩` metrics line after a
bash call that ran `pixel`, and the `pixel` skill: no model-callable Pixel tool,
no startup retrieval, no system-prompt block and no policy check on Pi's tool
calls.

## Install

`pixel setup` writes a local Pi package when Pi's agent directory
(`$PI_CODING_AGENT_DIR`, else `~/.pi/agent`) exists or `pi` is on `PATH`:

| Path | Content |
| --- | --- |
| `~/.local/share/pixel/pi-package/package.json` | the package manifest, naming its extensions |
| `~/.local/share/pixel/pi-package/extensions/pixel-impact.ts` | the command, bound to this machine's `pixel` binary |
| `~/.local/share/pixel/pi-package/extensions/pixel-brief.ts` | the brief hook and the metrics relay, bound to the same binary |
| `skills/pixel/SKILL.md` in Pi's agent directory | the `pixel` skill every harness gets |
| `settings.json` in Pi's agent directory | the package's absolute path, appended to `packages` |

Every other key and package in `settings.json` stays. Pi loads the package on
its next start; `/pixel-impact` then appears in its command list.

The install leaves Pi alone and reports yellow, rather than rewriting, when
`settings.json` does not parse or holds a `packages` that is not a list, or
when the agent path is not a directory. It no longer removes what earlier
releases wrote: a copy of the command in `extensions/pixel-impact.ts` of the
agent directory (Pi would register the name twice) and Pixel's block in
`APPEND_SYSTEM.md` are yours to delete.

`pixel doctor` reports the package as `install.pi-impact`: red when it is
missing from `packages` or bound to another binary, green when it is current or
Pi is absent. `pixel setup --remove` removes the package and its `packages` entry.

## The brief and the metrics line

`pixel-brief.ts` calls `pixel run-hook` and nothing else; it never gates a
prompt or a tool call and drops every failure silently.

- `before_agent_start` (a user prompt, not a custom message) runs `pixel
  run-hook task-event --provider pi --event prompt-submit` with the session id,
  the working directory and the prompt on stdin, plus `brief_only: true`: no
  task is bound and nothing is gated. The brief it returns is injected as the
  `pixel-brief` custom message, followed by the untrusted-data line. The brief
  gets the `brief_parts` budget (3 by default) and the 3 s hook window, and the
  call is cut off at 6.5 s.
- `session_start` and `session_compact` run the same hook with `session-start`:
  the repository's daemon is kept warm, and after a new session or a compaction
  the brief sends again what the context no longer holds.
- `tool_result` of a bash call whose command mentions `pixel` runs `pixel
  run-hook metrics --provider pi` and shows the finalized `🟩` line.

The `🟩 pixel prompt-submit` line of a delivered brief and the metrics line are
shown to you as a Pi notification (interactive and RPC modes; print and JSON
modes have none). They are not part of the model's context and are not saved
in the session. `PIXEL_METRICS=0` and `pixel config metrics off` silence both.

## The command

`/pixel-impact <symbol>` runs `pixel impact <symbol> --no-refresh --depth 2
--json --metrics off` once against the existing graph and shows the bounded
result (12 000 bytes at most) in the session without starting a turn. Missing,
stale, unsupported or slow results fall back to native search; the command
never builds or refreshes the index. If the graph is missing, run
`pixel rebuild-graph .` yourself.

## Per repository

`pixel setup --repo .` writes nothing for Pi. The project extension
`.pi/extensions/pixel-guard.ts` that earlier releases wrote is no longer
removed by `pixel setup`; delete it if it is still there.
