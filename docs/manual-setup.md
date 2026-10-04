# Manual Setup

Prefer to control your own setup, or using an agent `pixel install` does not
wire (Cursor, Copilot, ...)? You don't need `pixel install`.

`pixel install` does these things, and you can do each of them by hand:

1. **Deploy the agent system prompt** to `~/.local/share/pixel/agent-prompt.md`.
2. **Deploy the sub-agent prompt** to `~/.local/share/pixel/subagent-prompt.md`
   (a short version for Claude Code sub-agents, which see neither the session
   prompt nor its history).
3. **Add lifecycle hooks to `~/.claude/settings.json`**: a `SessionStart`
   hook injects the agent prompt as context into every Claude Code session
   that loads those user settings, however `claude` is launched on the
   machine where `pixel install` ran (a terminal, an IDE, an agent, cron).
   Another machine, such as a CI runner, has neither the hooks nor the
   binary until it is set up too ([In CI](#in-ci-claude-code-action)). No
   shell wrapper: an older install's `claude()` function in the shell
   profile is removed.
4. **Put the prompt into Codex's `config.toml`** as `developer_instructions`,
   so every Codex front end (CLI, desktop app, extension, sub-agents) gets it.
5. **Put the prompt into Pi's `~/.pi/agent/APPEND_SYSTEM.md`**, and, when
   their config directories exist, into OpenCode's global `AGENTS.md` and an
   Antigravity plugin under `~/.gemini/config/`.

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

```bash
claude --append-system-prompt-file ~/.local/share/pixel/agent-prompt.md
```

Sub-agents (the `Agent` tool: built-in agents, `.claude/agents/*.md`, agents
from a `--plugin-dir`) do not receive `--append-system-prompt-file`. In print
mode (`-p`/`--print`) Claude Code accepts a second flag for them; it is not
listed in `claude --help` and is ignored in interactive sessions. **It needs
Claude Code 2.1.261 or newer**: older releases exit with `error: unknown
option`, so check `claude --version` before adding it.

```bash
claude -p --append-system-prompt-file ~/.local/share/pixel/agent-prompt.md \
          --append-subagent-system-prompt-file ~/.local/share/pixel/subagent-prompt.md \
          "your task"
```

To make it automatic, do what `pixel install` does: register Pixel's
lifecycle hooks in `~/.claude/settings.json`. The `SessionStart` one prints
the deployed `agent-prompt.md` as `hookSpecificOutput.additionalContext`, so
every session that loads these user settings gets the prompt without a flag
or a shell function, whatever starts `claude` on this machine:

```json
{
  "hooks": {
    "SessionStart": [
      { "hooks": [{ "type": "command", "command": "'/path/to/pixel' run-hook session-start", "timeout": 10 }] }
    ]
  }
}
```

`pixel install` writes that entry with the absolute path of the `pixel` it
runs from, beside `UserPromptSubmit` (`run-hook prompt-submit --provider
claude`), `PostToolUse` on `Edit` (`run-hook post-tool-use --provider claude`)
and on `Bash` (`run-hook metrics --provider claude`, the 🟩 metrics relay)
and a `SessionStart` entry matched on `compact` (`run-hook post-compaction
--provider claude`), and leaves any hook of yours in place. Releases before
the hooks wrapped `claude` in a `claude()` function in `~/.zshrc`,
`~/.bashrc` or `~/.config/fish/conf.d/pixel.fish`; `pixel install` now
removes that block, and `pixel doctor` reports one that remains
(`install.legacy-wrappers`). The hook does not reach sub-agents: in print
mode, pass `--append-subagent-system-prompt-file` as above.

#### In CI (claude-code-action)

The action runs Claude Code on the runner, not on your machine. It reads the
runner's `~/.claude/settings.json`, merges its `settings` input into it and
loads the user, project and local setting sources, hooks included; but a
fresh runner has neither the `pixel` binary nor Pixel's hooks, since nobody
ran `pixel install` there. Two ways to give the run the prompt, both with
the binary installed in an earlier step:

- run `pixel install` in an earlier step of the job, so the runner's user
  settings carry the `SessionStart` hook the action then loads;
- or check the two prompt files into the repository (or copy them in an
  earlier step) and pass the flags through `claude_args`, which the action
  forwards to the Claude Code CLI:

```yaml
- uses: anthropics/claude-code-action@v1
  with:
    claude_args: >-
      --append-system-prompt-file .pixel-prompts/agent-prompt.md
      --append-subagent-system-prompt-file .pixel-prompts/subagent-prompt.md
```

Paths are relative to the checkout. Sub-agents forked from the main session
(`subagent_type: "fork"`) inherit the session prompt instead and are not
affected by the sub-agent flag.

### Codex

Codex takes the prompt through the `developer_instructions` key of
`~/.codex/config.toml` (`$CODEX_HOME/config.toml` when that variable is set).
The key is appended to Codex's developer message and leaves its own system
prompt in place; `model_instructions_file` looks similar but *replaces* that
system prompt (it becomes the base instructions, and the model loses its
native protocol and personality). A config key, unlike a shell function,
reaches the desktop app's bundled binary, the VS Code extension, scripts
that call the binary by path, and `spawn_agent` sub-agents (which inherit it
unless a role overrides it).

Codex has no file-backed variant of the key, so the prompt is embedded as a
TOML literal multi-line string, between two marker lines. Text you keep
outside the markers survives a re-install; `pixel install` refreshes only the
block, and `pixel uninstall` removes only the block (or the key, when nothing
else was in it):

```toml
developer_instructions = '''
Your own instructions, if any.

<!-- pixel:managed:begin -->
# Pixel Retrieval Layer — Mandatory Agent Protocol
... the content of ~/.local/share/pixel/agent-prompt.md ...
<!-- pixel:managed:end -->
'''
```

To do it by hand, paste `agent-prompt.md` between the markers. Codex reads the
file at session start, so a running session keeps the prompt it started with.
For a one-off session with different instructions, the CLI override wins:
`codex -c developer_instructions="..."`.

### Pi

Pi reads `~/.pi/agent/APPEND_SYSTEM.md` automatically — no flag needed:

```bash
mkdir -p ~/.pi/agent
printf '%s\n' 'Use the pixel tool for repository retrieval and repository Git workflows. Request the outcome through a stable action and goal. Pixel guidance is advisory by default.' >> ~/.pi/agent/APPEND_SYSTEM.md
pixel install --repo .
```

The repository install writes `.pi/extensions/pixel-guard.ts`, which Pi loads
after the project is trusted. The extension registers the structured `pixel`
tool and applies the [Pi policy](pi-harness.md) at `tool_call` time. Copying the
short prompt alone does not install the extension.

`pixel install --repo .` also adds a Pixel-managed block to the
repository-root `AGENTS.md`, for Codex and every other agent that reads that
file. It tells agents to try Pixel first for repository retrieval, but
does not block native tools: if Pixel is unavailable, the repository is not
indexed, or Pixel cannot answer, the agent can continue with native search or
file reading. Existing text before and after the managed block is preserved.

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
to opt into supported repository retrieval restrictions in Codex,
Antigravity, Pi, or Devin — the repository file by default, the machine-wide
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

Claude retains its native permission flow and supported compatible rewrites.
Codex and Antigravity use their own hook response contracts; a Pixel
recommendation does not grant host permissions. Composed Codex hooks still
honour foreign hook decisions even when Pixel's policy is off.

Shell syntax the policy cannot interpret reliably falls back to the original
command, including its complete pipeline or sequence. Enforcement is a
workflow preference, not a security sandbox. Pi's automatic context and
post-edit feedback operate independently of the policy mode.

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

## A note on prompt size

The system prompt is ~275 lines (~4 150 tokens). That is deliberate.

Pixel plays a central role: it replaces `grep`, `rg`, `git log -S`, `git blame`,
manual caller tracing, and exploratory file reading with a single indexed,
evidence-backed workflow. The prompt needs to map every native command to its
Pixel replacement and enforce the mandatory workflow phases — otherwise the
agent falls back to the slow, token-heavy habits the prompt exists to eliminate.

In practice the prompt pays for itself quickly: a single `pixel impact` call
can replace a dozen `grep` + file-read round trips, recovering the token cost
of the prompt in one command.

## Uninstall

Remove the prompt files, the Pi copy, the Claude Code hooks and the Codex
block:

```bash
rm -f ~/.local/share/pixel/agent-prompt.md
rm -f ~/.local/share/pixel/subagent-prompt.md
rm -f ~/.pi/agent/APPEND_SYSTEM.md
# then remove the `run-hook` entries from ~/.claude/settings.json and the
# pixel block from developer_instructions in ~/.codex/config.toml
```

Or just run `pixel uninstall`.
