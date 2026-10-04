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
Bash, curl, tar, git and either sha256sum or shasum. Container jobs need the
same tools. Download or index errors fail the action.

| Input | Default | Meaning |
| --- | --- | --- |
| `version` | required | Exact published release tag |
| `path` | `.` | Checkout directory, relative to the workspace or absolute |
| `prepare` | `true` | Set `false` to install only |

Outputs: `version`, absolute executable path `bin`, and `report`, the
absolute path to the preparation JSON (empty for install only). The action
sets `PIXEL_DAEMON_AUTO_START=0` for later steps. It does not install agent
hooks or rewrite the agent's configuration. For a subdirectory checkout,
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

Start without a cache. For large repositories, your workflow can restore
and save `<path>/.pixel` with `actions/cache`, keyed by runner OS, architecture,
Pixel version and checkout path. A daily suffix bounds cache churn; use the
same prefix as a restore key. Always run setup with `prepare: true` after a
restore to refresh the index for the current checkout. Save only after
successful preparation. Warm the cache on the default branch if other PRs
should reuse it. Do not share a writable `.pixel` directory between worktrees.
