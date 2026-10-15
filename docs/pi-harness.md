# Pixel in the Pi harness

Pi keeps its native tools. Pixel adds one explicit command, `/pixel-impact
<symbol>`, a `before_agent_start` hook that adds the `[PIXEL:BRIEF]` evidence
brief to each code prompt, and the `pixel` skill: no model-callable Pixel tool,
no startup retrieval, no system-prompt block and no policy check on Pi's tool
calls.

## Install

`pixel setup` writes a local Pi package when Pi's agent directory
(`$PI_CODING_AGENT_DIR`, else `~/.pi/agent`) exists or `pi` is on `PATH`:

| Path | Content |
| --- | --- |
| `~/.local/share/pixel/pi-package/package.json` | the package manifest, naming its extensions |
| `~/.local/share/pixel/pi-package/extensions/pixel-impact.ts` | the command, bound to this machine's `pixel` binary |
| `~/.local/share/pixel/pi-package/extensions/pixel-brief.ts` | the brief hook, bound to the same binary |
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
