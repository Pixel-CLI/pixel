# Manual Setup

Prefer to control your own setup, or using an agent `pixel setup` does not
wire (Copilot CLI, ...)? You don't need `pixel setup`.

`pixel setup` does these things, and you can do each of them by hand:

1. **Install the `pixel` skill**, `skills/pixel/SKILL.md`, in `~/.claude` and in
   each of `~/.codex`, `~/.cursor`, `~/.devin`, `~/.gemini` (Gemini CLI and
   Antigravity), `~/.pi/agent` and `~/.config/opencode` that exists. Pi's
   package writes the same file itself, and Antigravity's plugins carry their
   own copy. Copilot CLI and a generic `AGENTS.md` agent get no skill.
2. **Register hooks, preserving foreign ones.** Claude Code, Codex and Devin
   get the `task-event` hooks, whose prompt-submit entry adds the
   `[PIXEL:BRIEF]`, and Claude Code, Codex and Antigravity a metrics relay.
   Gemini CLI and OpenCode get a brief hook only.
3. **Install Pi's package when Pi is present**: the explicit `/pixel-impact`
   command, a hook that adds the brief on each prompt (the `task-event`
   prompt-submit entry, without the task ledger) and a relay that shows the
   metrics line after a `pixel` bash call (the same `metrics` entry Claude
   Code and Codex use).
4. **Store the settings and the first-party brief probes** under `~/.pixel/`.

No prompt, rules file or instruction block is written, and every agent keeps
its native tools.

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

In a terminal, `pixel setup` is a guided setup: it asks which of your agent CLIs
to write for and which pixel features to switch on, lists every file, hook and
setting it would write with the exact text, and writes nothing until you
confirm:

```bash
pixel setup              # this machine, and every agent it finds
pixel setup --repo .     # one repository: only .pixel/config.yaml
pixel setup --print      # the skill the default answers deploy, changing nothing
pixel setup --yes        # no questions, whatever the terminal
```

It installs the `pixel` skill for the agents you picked, registers their
hooks, and stores the settings the features you pick need. Without a terminal
(a pipe, CI) or with `--yes` or `--json` it does not ask: it deploys for every
configured host and reports what changed. `pixel setup --remove` takes the
skill, the hooks, the Pi package and the brief probes out again. The skill text
and the settings for the default answers are checked in under
[`tests/setup/.agents/`](../tests/setup/README.md), and the files written for
each agent are on the website's [per-agent pages](https://pixel-cli.dev/for/).

`pixel setup` deploys no prompt. To give an agent Pixel's full protocol
anyway, copy
[`crates/pixel-setup/assets/pixel-agent-prompt.md`](../crates/pixel-setup/assets/pixel-agent-prompt.md)
into that agent's own instruction file (see [Any other agent](#any-other-agent)).

### Claude Code

To assess callers or the impact of a known symbol, run `pixel impact
<symbol> --no-refresh` yourself; the per-prompt brief already runs it for a
prompt that names a symbol and asks to change it or find its callers.
Ordinary work keeps the usual tools.

`pixel setup` registers the `task-event` hooks in `~/.claude/settings.json`
(session, tool, subagent and `Stop` events; the `UserPromptSubmit` entries carry
the brief, `PIXEL_BRIEF=0` turns it off) and a `PostToolUse` metrics relay,
and adds the skill under `~/.claude/skills/pixel/`. Foreign hooks are
preserved. `pixel setup` no longer cleans up what earlier releases registered
(the `guard`, `session-start`, `prompt-submit`, `post-compaction` and
`post-tool-use` verbs of `run-hook`): delete those entries by hand. The binary
still answers them with exit 0, so a leftover does not block a tool call. Native
reads need no Pixel retrieval callback.

Copying the full prompt into Claude's instructions by hand enables a
different profile with additional context.

### Codex

For a known symbol whose callers or change impact matter, run `pixel impact
<symbol> --no-refresh`; the per-prompt brief covers the common case.

Global installation writes the same `task-event` suite and metrics relay into
`$CODEX_HOME/hooks.json` (default `~/.codex/hooks.json`), and the skill under
`~/.codex/skills/pixel/` when `~/.codex` exists. Codex runs a hook only after
you review it with `/hooks`. It adds no native-search rewrite or denial.

Do not copy the full agent prompt into `developer_instructions`. `pixel setup`
no longer removes a Pixel block an earlier release left in
`developer_instructions` or in a project `AGENTS.md`: delete it by hand (the
`<!-- pixel:managed:begin -->` and `<!-- pixel:setup:start -->` markers
delimit it). Existing sessions retain previously received context until a
fresh session.

### Pi

Pi uses a package, which is separate from the skill: the explicit
`/pixel-impact <symbol>` command requests bounded graph evidence, a
`before_agent_start` hook adds the brief and a `tool_result` hook shows the
metrics line. None replaces native tools. By
hand, add the package path to `packages` in Pi's `settings.json`. See [Pi
integration](pi-harness.md) for the files. The package step also writes the
skill to `~/.pi/agent/skills/pixel/SKILL.md`.

`pixel setup` no longer removes a Pixel block from
`~/.pi/agent/APPEND_SYSTEM.md` or the project extension
`.pi/extensions/pixel-guard.ts` that earlier releases wrote: delete them by
hand. `pixel setup --repo .` writes nothing for Pi.

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

The website's [per-agent pages](https://pixel-cli.dev/for/) list what
`pixel setup` writes for each agent it wires. To give any other tool the
full protocol, put the text of the bundled
`crates/pixel-setup/assets/pixel-agent-prompt.md` wherever that tool reads
always-on instructions: a rules file (`.cursor/rules`, `GEMINI.md`,
`.github/copilot-instructions.md`), a system-prompt flag, or a global
`AGENTS.md`. Copy it verbatim rather than a summary, and re-copy it after an
upgrade. `pixel setup` does not manage that copy, and `pixel doctor` does
not check it.

## Context and evaluation

More Pixel calls do not establish a benefit. See
[the arena evidence](../eval/arena/native-default-evidence.md) for measured
runs and their limits, including the explicit impact skill earlier releases
shipped and later removed.

## Uninstall

Run `pixel setup --remove`. It removes Pixel-owned registrations, the skill, the
Pi package and the brief probes while preserving unrelated configuration.
`pixel setup --repo` wrote only `<repo>/.pixel/config.yaml`; delete it by hand if
you no longer want it. Remove separately installed plugins through the host's
plugin manager. Do not delete a shared settings file merely because it once
contained Pixel text.
