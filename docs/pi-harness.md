# Pixel in the Pi harness

Pi keeps its native tools. Pixel adds one explicit command, `/pixel-impact
<symbol>`, and nothing that runs on its own: no model-callable Pixel tool, no
startup retrieval, no per-prompt context, no system-prompt block and no
policy check on Pi's tool calls.

## Install

`pixel install` writes a local Pi package when Pi's agent directory
(`$PI_CODING_AGENT_DIR`, else `~/.pi/agent`) exists or `pi` is on `PATH`:

| Path | Content |
| --- | --- |
| `~/.local/share/pixel/pi-package/package.json` | the package manifest, naming its one extension |
| `~/.local/share/pixel/pi-package/extensions/pixel-impact.ts` | the command, bound to this machine's `pixel` binary |
| `settings.json` in Pi's agent directory | the package's absolute path, appended to `packages` |

Every other key and package in `settings.json` stays. Pi loads the package on
its next start; `/pixel-impact` then appears in its command list.

The install leaves Pi alone and reports yellow, rather than rewriting, when
`settings.json` does not parse or holds a `packages` that is not a list, or
when the agent path is not a directory. A copy of the command an earlier
release wrote to `extensions/pixel-impact.ts` in the agent directory is
removed, so Pi does not register the name twice; a file there that Pixel did
not write is left and reported. The install also removes Pixel's retired block
from `APPEND_SYSTEM.md`, keeping any text of yours.

Installing the npm package as well (`pi install git:github.com/Pixel-CLI/pixel`)
registers the same command a second time, and Pi then lists `pixel-impact:1`
and `pixel-impact:2`: use one install path per machine.

`pixel doctor` reports the package as `install.pi-impact`: red when it is
missing from `packages` or bound to another binary, green when it is current or
Pi is absent. `pixel uninstall` removes the package and its `packages` entry.

## The command

`/pixel-impact <symbol>` runs `pixel impact <symbol> --no-refresh --depth 2
--json --metrics off` once against the existing graph and shows the bounded
result (12 000 bytes at most) in the session without starting a turn. Missing,
stale, unsupported or slow results fall back to native search; the command
never builds or refreshes the index. If the graph is missing, run
`pixel rebuild-graph .` yourself.

## Per repository

`pixel install --repo .` writes nothing for Pi. It removes the project
extension `.pi/extensions/pixel-guard.ts` (and the older `.pi/agent/` copy)
that earlier releases wrote, which carried task gates, a retrieval tool and a
native-tool policy. A file under that name that Pixel did not write stays.
`repo.pi-guard` in `pixel doctor .` is red while Pixel's copy is still there.
