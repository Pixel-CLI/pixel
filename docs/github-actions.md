# Pixel in GitHub Actions

Use the composite action before your CI agent or scripts. It downloads a
release, verifies its published SHA-256 checksum, adds `pixel` to `PATH`,
and builds text and code indexes without starting a daemon. No Rust
compiler, sudo, API key or agent subscription is needed for Pixel itself.

```yaml
name: Pixel
on: [pull_request]
permissions:
  contents: read
jobs:
  inspect:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v7
        with:
          fetch-depth: 0
      - uses: Pixel-CLI/pixel/.github/actions/setup-pixel@main
        with:
          version: v0.6.1
      - run: pixel what-changed --base "$PIXEL_BASE" --json
        env:
          PIXEL_BASE: ${{ github.event.pull_request.base.sha }}
```

The action first becomes available when its implementation merges; older
release tags do not contain it. Replace `@main` with the reviewed full commit
SHA for reproducible workflows (and pin checkout too). The action reference
selects the setup implementation; `version` separately selects the CLI release.
`version` is required and accepts only `vX.Y.Z`, never `latest`.

Supported runners: Linux x64, Linux ARM64 and macOS ARM64. Windows and Intel
macOS currently have no published Pixel binary. Self-hosted runners need
Python 3, Bash, curl, tar, git and either sha256sum or shasum. Container jobs need the
same tools. Download or index errors fail the action.

| Input | Default | Meaning |
| --- | --- | --- |
| `version` | required | Exact published release tag |
| `path` | `.` | Checkout directory, relative to the workspace or absolute |
| `prepare` | `true` | Set `false` to install only |
| `cache` | `false` | Restore/save indexes when preparation is enabled |
| `cache-version` | `1` | Change to invalidate existing caches |
| `minimum-symbols` | `0` | Require this graph size before saving a cache |
| `install-hooks` | `false` | Run global `pixel setup --yes --json` on the runner |

Outputs: `version`, absolute executable path `bin`, and `report`, the
absolute path to the preparation JSON (empty for install only). The action
sets `PIXEL_DAEMON_AUTO_START=0` for later steps. Agent hooks and configuration are unchanged unless `install-hooks: true` is set. For a subdirectory checkout,
set `path` here and `working-directory` on subsequent commands.

## Use in an agent review

Keep your existing agent action and credentials, and insert setup after
checkout and before the agent. Give the reviewer this instruction through
its normal prompt input:

> Pixel is available in the checked-out repository. Use `pixel what-changed --base <base-sha>`,
> `pixel impact <symbol>` and `pixel who-calls <symbol>` to investigate the
> affected code, then verify findings against source and relevant tests.
> Empty caller results do not prove that a symbol is unused.

Pass the PR base SHA to `what-changed`: its default only examines working-tree
changes, so a clean CI checkout needs an explicit base. Fetch full history
for diff and history analysis. Review the checkout you
intend: `pull_request` normally checks out GitHub's merge result; set the
checkout `ref` to the PR head SHA when your reviewer expects the exact head.
Use `pull_request` with read-only permissions for untrusted PRs; do not run
PR code under `pull_request_target` with secrets or a write token.

If Pixel is optional, set `continue-on-error: true` on its setup step and
check `steps.<id>.outcome` before asking the agent to use it. An installed
binary alone does not prove preparation succeeded.

## Index reuse

Set `cache: 'true'` to let this action restore and save text shards, their
state and the code graph (including SQLite journals). Keys include the
runner OS/architecture, CLI version, checkout path and `cache-version`,
with a UTC daily suffix and a matching prefix for older caches. Preparation
always runs, including on exact hits. Cache service errors are non-fatal;
preparation and minimum-symbols failures are fatal and prevent saving.
`cache-hit` and `cache-matched-key` expose the restore result.

Only rebuildable text/graph files are cached: never all of `.pixel`, hooks,
configuration, plugins, task state or action logs. History, vectors and the
auxiliary MVP index are rebuilt on demand. Do not run another Pixel writer
while setup executes. Cache contents contain repository source: GitHub's
branch/PR cache access rules apply. Warm the default branch for reuse by PRs.
Use `cache-version` to recover from a damaged cache. No cache is used when
`prepare: 'false'`. Remove downstream cache steps when enabling this option.

## Agent hooks in CI

On an ephemeral runner, add `install-hooks: 'true'` before launching the
agent. This invokes the pinned CLI's **global** setup: hooks, skills and bundled
probes. By default, it includes Claude, Codex and other configured hosts.
Set `hooks-agent: claude`, `hooks-agent: codex` or `hooks-agent: claude,codex`
to select integrations. Supported names are `claude`, `codex`, `devin`,
`cursor`, `gemini`, `antigravity`, `opencode`, `pi` and `agents-md`.
The CLI accepts `pixel setup --agent claude,codex --yes` or repeated
`--agent` flags; omitting the flag preserves automatic setup. Cursor and
`agents-md` install skills, not native event hooks. Gemini and Antigravity
share their skill directory. Existing host-detection rules still apply
(for example Pi needs its agent directory or an installed CLI).
Explicit selection requires a release with `setup --agent`; v0.8.0 is rejected
before setup writes any files. `setup --repo` does not install these hooks. Use a CLI
release supporting `setup --yes --json` (the smoke workflow uses v0.8.0).

```yaml
- uses: Pixel-CLI/pixel/.github/actions/setup-pixel@main # Pin a reviewed full SHA
  with:
    version: v0.8.0
    cache: 'true'
    install-hooks: 'true'
    hooks-agent: all # Or a list such as claude,codex with a supporting release
- uses: anthropics/claude-code-action@v1
  with:
    claude_code_oauth_token: ${{ secrets.CLAUDE_CODE_OAUTH_TOKEN }}
    prompt: Review the changes using Pixel and verify findings against source.
```

Setup merges its hooks into the runner user's settings; the binary path points
into runner temporary storage. Opt in only on disposable runners: settings
persist on self-hosted machines but the temporary binary may not. Hook setup
failure fails the action. Installation is not proof that a host invoked a hook;
check the agent transcript/Pixel observations before claiming runtime coverage.
Claude's action preserves existing user settings when no `settings` override
replaces `hooks`; its settings merge is shallow. Keep worktree-index copying
and subagent instructions where your review workflow needs them.

For Rails, exclude `vendor/bundle` before setup and pass your repository's
`minimum-symbols` threshold (Yespark uses 1000). That check runs before cache
publication; downstream reporting can still consume `report`.

Design references: [actions/cache strategies](https://github.com/actions/cache/blob/main/caching-strategies.md),
[ruby/setup-ruby cache inputs](https://github.com/ruby/setup-ruby/blob/master/action.yml),
and [Claude settings merge](https://github.com/anthropics/claude-code-base-action/blob/main/src/setup-claude-code-settings.ts).

The smoke workflow executes the installed SessionStart and UserPromptSubmit commands in an isolated home. It verifies task-bridge receipts and brief output, so a fail-open exit code alone cannot pass. The source lane executes Claude-only, Codex-only and combined installations, verifying preservation of other agent settings and idempotence; no model credentials are needed.
