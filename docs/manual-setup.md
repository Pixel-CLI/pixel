# Manual Setup

Prefer to control your own setup, or using an agent `pixel install` does not
wire (Cursor, Copilot, ...)? You don't need `pixel install`.

`pixel install` does these things, and you can do each of them by hand:

1. **Keep Claude and Codex task lifecycle hooks**, preserving foreign hooks,
   while removing Pixel's automatic retrieval prompts and metrics callbacks.
2. **Remove retired Pixel instruction blocks** from Codex configuration and
   project `AGENTS.md`, preserving unrelated text.
3. **Install Pi's explicit `/pixel-impact` package when Pi is present** and
   remove Pixel's automatic Pi prompt.
4. **Remove what earlier releases wrote for every other agent**: deployed
   prompts, guard hooks and plugins. It wires none of them, and every agent
   keeps its native tools.

The Codex and Claude plugins ship no skill and register no hooks; the
per-prompt evidence brief comes from the task hook `pixel install` writes.
The CLI and the plugins are separate installations.

## 1. Install the binary

```bash
curl -fsSL https://github.com/Pixel-CLI/pixel/releases/latest/download/install.sh | sh
```

The script picks the archive for this machine (Linux x86_64, Linux arm64, or Apple Silicon), checks its SHA-256, and runs the binary once. `PIXEL_INSTALL_DIR` picks the directory (default `~/.local/bin`); From the release after 0.7.0, `DESTDIR` stages the install under another root, `$DESTDIR$PIXEL_INSTALL_DIR`, for a package build; the 0.7.0 script ignores it and writes to `PIXEL_INSTALL_DIR` itself. On GitHub Actions, also run `echo "${PIXEL_INSTALL_DIR:-$HOME/.local/bin}" >> "$GITHUB_PATH"` — needed for the `install.sh` already published on the current release; this branch's script appends itself, so drop the echo after the next release.

Or build from source:

```bash
cargo build --release -p pixel-cli
cp target/release/pixel ~/.local/bin/pixel
```

To update an existing install later, prefer `pixel self-update` over a manual
copy: it rebuilds, installs over the binary that is actually running (an
install behind a shim, `~/.cargo/bin`, or `~/.local/bin/pixel`), uses an
atomic rename, stops the repo's daemon, and warns when another `pixel`
earlier on PATH would still shadow it. It refuses a binary that mise or
Homebrew installed: update those with `mise`/`brew`, try a local build with
`pixel self-update --dev` (installed as `pixel-dev`), or pass
`--install-path` to overwrite on purpose.

## 2. Wire it into your agent

`pixel install` deploys no prompt. To give an agent Pixel's full protocol
anyway, copy
[`crates/pixel-install/assets/pixel-agent-prompt.md`](../crates/pixel-install/assets/pixel-agent-prompt.md)
into that agent's own instruction file (see [Any other agent](#any-other-agent)),
not under `~/.local/share/pixel/`: `pixel install` deletes the
`agent-prompt.md` and `subagent-prompt.md` earlier releases deployed there.

### Claude Code

To assess callers or the impact of a known symbol, run `pixel impact
<symbol> --no-refresh` yourself; the per-prompt brief already runs it for a
prompt that names a symbol and asks to change it or find its callers.
Ordinary work keeps the usual tools. The native Pixel plugin ships no skill,
injects no session prompt and registers no retrieval hooks.

`pixel install` removes its retired retrieval, post-edit advice, compaction
and metrics registrations from Claude's user settings. Independent task-event
hooks remain available for configured task contracts. Repository installation
preserves foreign hooks and restores adopted RTK registrations, removing the
Pixel retrieval wrapper. Native reads need no Pixel retrieval callback.

Copying the full prompt into Claude's instructions by hand enables a
different profile with additional context.

### Codex

For a known symbol whose callers or change impact matter, run `pixel impact
<symbol> --no-refresh`; the per-prompt brief covers the common case. The
native plugin ships no skill, and its manifest registers no retrieval hooks.

Global installation removes Pixel's old retrieval `UserPromptSubmit` and
metrics hooks and retains the independent task-event suite. The previous
caller-facts prompt classifier has been retired. Project installation
restores foreign hook registrations from its owned composed-guard backup and
removes the retrieval wrapper. It adds no native-search rewrite or denial.

Do not copy the full agent prompt into `developer_instructions`.
Installation removes older Pixel-managed blocks from global and project
Codex configuration and project `AGENTS.md`, preserving foreign text.
Existing sessions retain previously received context until a fresh session.

### Pi

Pi uses a package. The explicit
`/pixel-impact <symbol>` command requests bounded graph evidence. It does
not query Pixel at startup, classify every prompt, or replace native tools.
By hand, add the package path to `packages` in Pi's `settings.json`, or
install the repository as a Pi package (`pi install git:github.com/Pixel-CLI/pixel`),
not both. See [Pi integration](pi-harness.md) for the files.

`pixel install` removes its managed and recognized historical automatic
instructions from `~/.pi/agent/APPEND_SYSTEM.md`, preserving user text.
`pixel install --repo .` removes the retired project extension
`.pi/extensions/pixel-guard.ts`. No Pixel-first block is added to project
`AGENTS.md`.

### Bounded graph queries

The Pi extension and the per-prompt brief use:

```bash
pixel impact 'knownSymbol' --no-refresh --depth 2 --json --metrics off
```

Use depth 1 when only direct callers are needed. `--no-refresh` checks the existing
graph and source signature, limits the query to 1500 ms and the serialized
result to 32 KiB, and does not start a daemon or refresh indexes. Only files
changed since the last full graph build are re-hashed, so an edit that also
restores an older mtime is not detected. Missing, stale or incompatible data
produces an error so the caller can continue with native tools; an ambiguous
name returns the matching `candidates` with their uids, as the daemon path
does. It cannot be combined with `--workspace`.
Older binaries that lack this flag should be treated as unavailable for
this capability, not repaired during an ordinary task.

Graph results remain incomplete candidates. Verify relevant source and
look beyond the returned list when task correctness requires it.

### Any other agent

`pixel install` wires only the agents above; the website's
[per-agent pages](https://pixel-cli.dev/for/) list what it writes or removes
for each, and the plugins or rules files for the others. To give any tool the
full protocol, put the text of the bundled
`crates/pixel-install/assets/pixel-agent-prompt.md` wherever that tool reads
always-on instructions: a rules file (`.cursor/rules`, `GEMINI.md`,
`.github/copilot-instructions.md`), a system-prompt flag, or a global
`AGENTS.md`. Copy it verbatim rather than a summary, and re-copy it after an
upgrade. `pixel install` does not manage that copy, and `pixel doctor` does
not check it.

## Renamed commands

If you wrote Pixel commands into your own instructions before the rename
(a `CLAUDE.md`, a rules file, a CI script, a hand-wired hook such as
`<pixel> hook session-start`), you do not have to rewrite them yet: each old
name is a hidden alias of its new name until 1.0, and `pixel doctor` accepts
rule text that still uses the old names. Update them when convenient, using
the table in [renamed-commands.md](renamed-commands.md).

An old name prints one `note:` line on stderr with the new name. It never
touches stdout or `--json` output, and `--metrics off` or `PIXEL_METRICS=0`
silence it together with the metrics line. Hook invocations stay silent.

## Context and evaluation

More Pixel calls do not establish a benefit. See
[the arena evidence](../eval/arena/native-default-evidence.md) for measured
runs and their limits, including the explicit impact skill earlier releases
shipped and later removed.

## Uninstall

Run `pixel uninstall` (and `pixel uninstall --repo .` for repository
integration). It removes Pixel-owned registrations and managed text while
preserving unrelated configuration. Remove separately installed plugins
through the host's plugin manager. Do not delete a shared settings or
`APPEND_SYSTEM.md` file merely because it once contained Pixel text.
