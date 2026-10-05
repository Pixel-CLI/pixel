# Manual Setup

Prefer to control your own setup, or using an agent `pixel install` does not
wire (Cursor, Copilot, ...)? You don't need `pixel install`.

`pixel install` does these things, and you can do each of them by hand:

1. **Deploy CLI integration assets** under `~/.local/share/pixel/`.
2. **Keep Claude and Codex task lifecycle hooks**, preserving foreign hooks,
   while removing Pixel's automatic retrieval prompts and metrics callbacks.
3. **Remove retired Pixel instruction blocks** from Codex configuration and
   project `AGENTS.md`, preserving unrelated text.
4. **Install Pi's explicit impact extension when Pi is configured** and remove
   Pixel's automatic Pi prompt. Repository task controls remain separate.
5. **Keep other provider integrations** on their documented paths. The focused
   skills pilot does not establish behavior or performance for every host.

Native Codex and Claude plugins distribute a small, explicit-only
`pixel-impact` skill. They do not register automatic retrieval hooks.
The CLI and skill/plugin are separate installations.

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

## 2. Copy the system prompt

The prompt is bundled in the repo at
[`crates/pixel-install/assets/pixel-agent-prompt.md`](../crates/pixel-install/assets/pixel-agent-prompt.md).

```bash
mkdir -p ~/.local/share/pixel
cp crates/pixel-install/assets/pixel-agent-prompt.md ~/.local/share/pixel/agent-prompt.md
cp crates/pixel-install/assets/pixel-subagent-prompt.md ~/.local/share/pixel/subagent-prompt.md
```

## 3. Wire it into your agent

### Claude Code

Use the native Pixel plugin's `pixel-impact` skill explicitly when assessing
callers or the impact of a known symbol. Its description and instructions
are separated from the legacy full agent prompt; ordinary work keeps the
usual tools. The plugin does not inject a session prompt or register retrieval
hooks. It requires a compatible Pixel binary on PATH.

`pixel install` removes its retired retrieval, post-edit advice, compaction
and metrics registrations from Claude's user settings. Independent task-event
hooks remain available for configured task contracts. Repository installation
preserves foreign hooks and restores adopted RTK registrations, removing the
Pixel retrieval wrapper. Native reads need no Pixel retrieval callback.

The deployed full prompts remain available for explicitly selected legacy
integrations. Copying or appending them manually enables a different profile
with additional context; it is not the focused-skill configuration.

### Codex

Use `$pixel-impact` explicitly for a known symbol whose callers or change
impact matter. Automatic selection stays disabled until paired evaluation
supports enabling it. The native plugin declares only this focused skill;
its default manifest registers no retrieval hooks.

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

Pi uses an extension rather than a duplicated skill. The explicit
`/pixel-impact <symbol>` command requests bounded graph evidence. It does
not query Pixel at startup, classify every prompt, or replace native tools.
See [Pi integration](pi-harness.md) for the extension and task-control modes.

`pixel install` removes its managed and recognized historical automatic
instructions from `~/.pi/agent/APPEND_SYSTEM.md`, preserving user text.
`pixel install --repo .` maintains the separate repository task adapter;
retrieval bootstrap and automatic post-edit advice are disabled by default.
No Pixel-first block is added to project `AGENTS.md`.

### Bounded graph queries

The skill and extension use:

```bash
pixel impact 'knownSymbol' --no-refresh --depth 2 --json --metrics off
```

Use depth 1 when only direct callers are needed. `--no-refresh` checks the existing
graph and source signature, limits the query to 1500 ms and the serialized
result to 32 KiB, and does not start a daemon or refresh indexes. Missing,
stale, incompatible or ambiguous data produces an error so the caller can
continue with native tools. It cannot be combined with `--workspace`.
Older binaries that lack this flag should be treated as unavailable for
this capability, not repaired during an ordinary task.

Graph results remain incomplete candidates. Verify relevant source and
look beyond the returned list when task correctness requires it.

### Antigravity CLI (agy)

In an indexed workspace, Pixel's `PreInvocation` hook extracts search terms
from the initial user request, runs `pixel search-content` itself, and sends
the actual matches to the same model invocation as an `ephemeralMessage`.
The search runs before the model can call native retrieval tools. It is
limited to 20 matching lines, 64 KiB of output and five seconds. Missing
request terms, unavailable indexes, failed searches and empty results leave
native retrieval available without a hook error or denial.

AGY 1.2.13 documents `toolCall` injection but rejects Pixel's injected
`run_command` with `unknown injected step type: <nil>`. Pixel therefore uses
the [documented string-message response](https://antigravity.google/docs/hooks?tab=ide)
to deliver a search that has already executed. Plugin installation or injected
instructions alone do not prove retrieval: verify a fresh session's hook
output and tool order. `pixel doctor` checks the `PreInvocation` registration
in the global hooks and both installed plugin copies.

### Optional retrieval enforcement

Pixel's retrieval policy defaults to advice. Run `pixel config policy enforce`
to opt into supported repository retrieval restrictions in
Antigravity or Devin — the repository file by default, the machine-wide
`~/.pixel/config.yaml` with `--global` — or set `PIXEL_POLICY=enforce` in the
environment that launches the agent to override every file layer.
`pixel config policy` reports the effective value and the layer that set it.
Devin's installed project hook keeps supported `exec` rewrites enabled by
default, while native `read`, `grep` and `glob` calls proceed without a hook
denial. The hook cannot silently turn a native tool call into `exec`; use the
injected Pixel workflow instructions to steer retrieval, or enforce the policy
if visible denials are acceptable. The simple `cat`, `ls` and `find` forms
Pixel can map are rewritten too; larger reads and unsupported syntax are not
guessed. Use `pixel config policy off` to disable policy decisions and
rewrites. The existing `PIXEL_TARGETS_GUARD=0` (also `false` or `off`) remains
an opt-out. Restart Devin after changing its environment.

Claude and Codex retain native retrieval and permissions under every retrieval
policy mode. Antigravity uses its own hook response contract; a Pixel
recommendation does not grant host permissions. Composed Codex hooks still
honour foreign hook decisions even when Pixel's policy is off.

Shell syntax the policy cannot interpret reliably falls back to the original
command, including its complete pipeline or sequence. Enforcement is a
workflow preference, not a security sandbox. Pi task contracts remain
independent of retrieval policy.

### Any other agent

`pixel install` covers the agents above, plus OpenCode and Antigravity when
their config directories exist; the website's
[per-agent pages](https://pixel-cli.dev/for/) list what it writes
for each, and the plugins or rules files for the others. For any other tool, put
the full text of `~/.local/share/pixel/agent-prompt.md` wherever that tool
reads always-on instructions: a rules file (`.cursor/rules`, `GEMINI.md`,
`.github/copilot-instructions.md`), a system-prompt flag, or a global
`AGENTS.md`. Copy the bundled prompt verbatim rather than a summary; it is
the single source of truth, and `pixel doctor` checks the deployed copy
against it. Re-copy it after each `pixel self-update`.

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

A discoverable skill can have metadata cost even when its body is not loaded.
The focused skill stays explicit-only by default. Automatic activation is
evaluated separately from command execution and task quality; more Pixel
calls do not establish a benefit. See
[the arena evidence](../eval/arena/native-default-evidence.md) for measured
runs and their limits.

## Uninstall

Run `pixel uninstall` (and `pixel uninstall --repo .` for repository
integration). It removes Pixel-owned registrations and managed text while
preserving unrelated configuration. Remove separately installed plugins
through the host's plugin manager. Do not delete a shared settings or
`APPEND_SYSTEM.md` file merely because it once contained Pixel text.
