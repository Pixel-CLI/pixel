---
title: "Documentation"
description: "Install Pixel, wire it into your agents, and read what its answers promise and what they do not."
---

<!-- Every `pixel <command>` quoted here must exist: crates/pixel/tests/cli/docs_drift.rs reads this page. The per-repository list must name exactly the files `pixel install --repo` writes (REPO_ARTIFACTS). Keep it in step with ARCHITECTURE.md. -->

## Install

Pixel is a single binary for macOS and Linux. The install script is one line:

```bash
curl -fsSL https://github.com/Pixel-CLI/pixel/releases/latest/download/install.sh | sh
```

It runs [`scripts/install.sh`](https://github.com/Pixel-CLI/pixel/blob/main/scripts/install.sh), which every release publishes as an asset: one POSIX `sh` file, no `sudo`. It picks the archive for the machine it is running on, refuses that archive unless its SHA-256 matches the release's checksum, runs the binary once, and writes a single file into `$PIXEL_INSTALL_DIR` (default `~/.local/bin`). From the release after 0.7.0, a `DESTDIR` set for a package build stages that file under `$DESTDIR$PIXEL_INSTALL_DIR` instead; the 0.7.0 script ignores `DESTDIR`. It edits no shell profile.

| Machine | Archive the script downloads |
| --- | --- |
| Linux x86_64 (`ubuntu-latest`) | `pixel-<tag>-x86_64-unknown-linux-musl.tar.gz` |
| Linux arm64 | `pixel-<tag>-aarch64-unknown-linux-musl.tar.gz` |
| Apple Silicon | `pixel-<tag>-aarch64-apple-darwin.tar.gz` |

To read it before it runs:

```bash
curl -fsSL -o install.sh https://github.com/Pixel-CLI/pixel/releases/latest/download/install.sh
less install.sh    # every line it will run
sh install.sh
```

### GitHub Actions

The same script. The second line is for the `install.sh` already published on the current release, which does not yet append to `GITHUB_PATH`; this branch's script does, so drop the echo after the next release (a duplicate line is harmless meanwhile).

```yaml
- name: Install Pixel
  run: |
    curl -fsSL https://github.com/Pixel-CLI/pixel/releases/latest/download/install.sh | sh
    echo "${PIXEL_INSTALL_DIR:-$HOME/.local/bin}" >> "$GITHUB_PATH"
```

New installs use the script; re-running the same `curl … | sh` line updates an existing one. A binary managed by Homebrew or mise keeps being upgraded by its manager.

To build from source, see [CONTRIBUTING.md](https://github.com/Pixel-CLI/pixel/blob/main/CONTRIBUTING.md).

Then let your agents use it, and check the result:

```bash
pixel install         # once, from anywhere
pixel prepare-repo .  # optional: index, graph and a warm daemon for this repository
pixel doctor .        # optional: health check
pixel list-signatures path/to/a/large/file   # first result: full read vs Pixel, in tokens
```

The index, the code graph and the optional history data live in `.pixel/` at the repository root and never leave the machine, and there is no telemetry. The network is used only for Git remote operations, the optional `pixel classify` and `pixel web-search`, the embedding model downloaded from Hugging Face on first use, and a once-a-day release check for a person at a terminal (`PIXEL_NO_UPDATE_CHECK=1` turns it off) ([security model](https://github.com/Pixel-CLI/pixel/blob/main/SECURITY.md)).

## What pixel install wires

`pixel install` is global: run it once, from anywhere. It retains prompt assets under `~/.local/share/pixel/` for legacy/manual integrations. Codex and Claude keep native retrieval with separate task lifecycle hooks; Pi exposes an explicit impact command:

{{% agents-install %}}

Each agent's page under [For your agent](../for/) names the files, the check and the removal, including the agents `pixel install` leaves alone.

At a terminal, `pixel install` opens with a short animation before its summary: an agent grepping its way through a repository, then the same task in three Pixel calls. Any key skips it; `PIXEL_NO_INTRO=1` (or `NO_COLOR`, `CI`, `--json`, a pipe, a `TERM` of `dumb` or unset, or a terminal narrower than 64 columns or shorter than 18 rows) turns it off.

`pixel uninstall` removes everything `pixel install` wrote, and the binary at `~/.local/bin/pixel`, where the install script puts it. A package manager removes its own copy: uninstall with the manager that owns it (`mise uninstall pixel`, or the Homebrew equivalent).

### Per-repository guards

`pixel install --repo <path>` updates project-local integration and skips global steps:

- `<repo>/.claude/settings.local.json` and `.claude/settings.json`: removes owned retrieval guards and restores RTK hook groups from `<repo>/.claude/pixel-rtk-hooks.json`, preserving foreign hooks and independent task controls
- `<repo>/.codex/config.toml`: removes retired Pixel `developer_instructions` while preserving foreign text
- `<repo>/.codex/hooks.json`: removes owned retrieval registrations and restores adopted hooks from `<repo>/.codex/pixel-composed-guard-backup.json` when the managed snapshot still matches; preserves user changes and leaves Git-tracked hook files alone
- `<repo>/.devin/config.local.json`: the guard hook for Devin
- `<repo>/.pi/extensions/pixel-guard.ts`: Pi's task lifecycle adapter, loaded once Pi trusts the project
- `<repo>/AGENTS.md`: removes the retired managed Pixel-first block; surrounding instructions are preserved

Machine-specific artifacts that name this machine's `pixel` binary are listed in the clone's `.git/info/exclude`, so a `git add -A` cannot publish them. `.codex/config.toml` and the root `AGENTS.md` are portable and do not name the local binary.

Codex and Claude leave retrieval native under every shared policy setting. Pi's default integration adds an explicit `/pixel-impact <symbol>` command; the project adapter retains task controls. Legacy Pi retrieval behavior requires `PIXEL_PI_RETRIEVAL=1`. Other providers retain their existing policy settings. [Pi integration and exceptions](https://github.com/Pixel-CLI/pixel/blob/main/docs/pi-harness.md) describe the boundary. `pixel doctor <repo>` checks global and project artifacts.

## Updating

Upgrading replaces the binary only. The agent prompt and the per-agent config keys belong to you, not to the package manager, so they keep the old release's text until you refresh them.

| Installed with | Upgrade the binary |
| --- | --- |
| mise | `mise upgrade pixel` |
| `install.sh` | run the same `curl … \| sh` line again |
| Source checkout | `pixel self-update` rebuilds and reinstalls the running binary |

Then, whatever the channel:

```bash
pixel install
pixel doctor . --fix   # runs each repair a flagged check names, then re-checks
```

`pixel doctor .` reports the wiring as stale until you do, with the command that repairs each finding, and exits 1 while a check is red.

## Plugins

Codex and Claude plugins provide an explicit impact skill without automatic retrieval hooks. Pi's package provides `/pixel-impact <symbol>`. The binary must be installed separately. These focused integrations make one bounded query against an existing fresh graph; missing binaries, unsupported versions or stale indexes fall back to native tools. Other agent packages retain their existing protocol integration.

| Tool | Install |
| --- | --- |
| Claude Code | `/plugin marketplace add Pixel-CLI/pixel`, then `/plugin install pixel@pixel` |
| Codex | `codex plugin marketplace add Pixel-CLI/pixel`, then `codex plugin add pixel@pixel` |
| Copilot CLI | `copilot plugin marketplace add Pixel-CLI/pixel`, then `copilot plugin install pixel@pixel` |
| Devin | add `github.com/Pixel-CLI/pixel` as a Devin plugin |
| Pi | `pi install git:github.com/Pixel-CLI/pixel` |
| Cursor, Kiro, Cline, Qoder | rules ship under `.cursor/rules/`, `.kiro/steering/`, `.clinerules/` and `.qoder/rules/`: copy them into your project |

OpenCode has no Pixel plugin package published yet, so it is not in the table: `pixel install` puts the protocol in its global `AGENTS.md` instead ([Pixel for OpenCode](../for/opencode/)).

Any other agent: paste [`PIXEL.md`](https://github.com/Pixel-CLI/pixel/blob/main/PIXEL.md), the plain-Markdown protocol, into whatever instruction surface it offers. [Manual setup](https://github.com/Pixel-CLI/pixel/blob/main/docs/manual-setup.md) covers wiring the full prompt by hand.

## The workflow

Codex and Claude use native search and editing by default. Choose Pixel commands when their facts help with a specific task:

```bash
pixel scope-task "<task>"        # optional task scope and candidate targets
pixel find-code "<phrase>"       # optional lookup by behavior
pixel impact "<symbol>" --no-refresh # explicit impact query using an existing fresh graph
pixel what-changed               # inspect what already differs
pixel review-changes             # the working tree, structured
pixel commit-and-push --files <f1> --files <f2> -m "msg" --request-id "id" origin HEAD
```

An impact query is optional and requires a known symbol. Inspect its cited source and continue with native tools when the graph is unavailable, stale, ambiguous or unhelpful. The agent commits or pushes only when asked; commit operations use a `--request-id` for crash-safe, idempotent execution.

{{< workflow-jobs >}}

Optional model-backed decisions have a [classification guide](../classify/).

## Commands

The most used commands, by job. `pixel --help` lists all of them, and [ARCHITECTURE.md](https://github.com/Pixel-CLI/pixel/blob/main/ARCHITECTURE.md#command-surface) describes each in one line.

### Find code

| Instead of | Run |
| --- | --- |
| `grep`, `rg` | `pixel search-content "re" [path]`: the same regex, indexed and capped |
| grep for a function by name | `pixel find-code "name"`: a phrase resolved through the concept index |
| grep for a definition | `pixel find-symbol "Foo"`: the exact symbol, from the code graph |
| "how is auth handled?" | `pixel search-meaning "how is auth handled?"`: semantic, not regex |
| reading a whole file | `pixel list-signatures <file>` or `pixel pack-context <uid>`: the skeleton, or one symbol fitted to a budget |

### Scope and impact

| Question | Run |
| --- | --- |
| Which files does this task touch? | `pixel scope-task "task"` |
| What breaks if I change this? | `pixel impact "symbol"` |
| Who calls it, what does it call? | `pixel who-calls "X" --role callers` |
| How does A reach B? | `pixel call-path "A" "B"` |
| What did I already change? | `pixel what-changed` |
| A checklist for a multi-file fix | `pixel plan "task"` |

### History

| Instead of | Run |
| --- | --- |
| `git log -S "x"` | `pixel dig-history --phrase "x"` |
| `git log --grep "x"` | `pixel search-history "x"` |
| `git log --follow f` | `pixel file-history --file f` |
| `git blame f` | `pixel who-wrote f` |
| "it worked before" | `pixel plan-rollback "<problem>"`: flags the breaking commit, writes nothing without `--apply` |

### Git changes

| Instead of | Run |
| --- | --- |
| `git status` | `pixel repo-state` |
| `git diff` | `pixel review-changes` |
| `git log --oneline` | `pixel commit-history` |
| `git branch -a -vv` | `pixel list-branches` |
| `git pull --rebase` | `pixel sync-branch` |
| `git add` and `git commit` | `pixel commit --files <f1> --files <f2> -m "msg" --request-id "id"` |
| `git push` | `pixel push`: a leased push, never a raw `--force` |

### Past sessions

`pixel recall` searches the transcripts of every agent on the machine (Claude Code, Codex, Pi and others). `pixel recall search "token"` finds an exact string, `pixel recall ask "topic"` a topic in your own words, and `pixel recall show <ref> --turn N..M` reads the turns around a hit.

## Reading the answers

Every result carries a marker set by the system, not by the model:

- `complete`: every match was returned.
- `capped`: the answer was truncated and more matches may exist. Narrow the pattern or the path.
- `unresolved`: nothing was found. Try another query, or `pixel search-meaning`.

Graph answers (`pixel impact`, `pixel who-calls`, `pixel call-path`) also carry an `epistemics` object:

- `closed_world` is always `false`. Static analysis is never complete, so "0 callers" means none were found, not that none exist.
- `lower_bound: true` flags same-name call sites the resolver could not settle: more edges may exist.
- `extraction_limits` names the known blind spots: callbacks passed as arguments, dynamic dispatch, macro-generated calls, `eval`.

### When native tools are right

Pixel does not cover every job. Use the native command for grep flags Pixel lacks (`-l`, `-m`), pipelines, files outside the index (git-ignored, binary, or over 4 MiB), in-place edits with `sed`, interactive Git such as `rebase -i` and `stash`, and network operations such as `clone`.

## Token savings

`pixel token-savings` reports, for the retrieval commands you ran, the fraction of the candidate pool the agent did not have to read. It measures what reached the agent's context, not your invoice. The replay of [shunt](https://github.com/spotify/portal-ai-plugins/tree/main/plugins/shunt)'s benchmark on Pixel's own repository is on the [benchmarks page](../benchmarks/#reading-code), with its method.

Each Pixel command also prints a `🟩 Pixel` line on stderr with its measured duration and two estimates: tokens saved against the native workflow, and time saved against sequential round trips. Both are estimates, and zero or negative values are valid. `--metrics=off` or `PIXEL_METRICS=0` turns the line off.

One command measures instead of estimating: `pixel list-signatures <file>` stands in for reading that file, so its line compares the file with the outline it printed, as `full read 10365 tok, pixel answer 641 tok (-94%)` (Requests' `models.py`). Both counts are bytes divided by four, rounded down, the method of the [benchmarks page](../benchmarks/#well-known-files), and it works on a fresh clone with no session behind it.
