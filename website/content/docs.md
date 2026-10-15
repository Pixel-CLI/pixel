---
title: "Documentation"
description: "Install Pixel, wire it into your agents, and read what its answers promise and what they do not."
---

<!-- Every `pixel <command>` quoted here must exist: crates/pixel/tests/cli/docs_drift.rs reads this page. The "Per repository" section must say what `pixel setup --repo` writes (only `<repo>/.pixel/config.yaml`). Keep it in step with ARCHITECTURE.md and the README. -->

## Install

Pixel is a single binary for macOS and Linux. The install script is one line:

```bash
curl -fsSL https://github.com/Pixel-CLI/pixel/releases/latest/download/install.sh | sh
```

It runs [`scripts/release/install.sh`](https://github.com/Pixel-CLI/pixel/blob/main/scripts/release/install.sh), which every release publishes as an asset: one POSIX `sh` file, no `sudo`. It picks the archive for the machine it is running on, refuses that archive unless its SHA-256 matches the release's checksum, runs the binary once, and writes a single file into `$PIXEL_INSTALL_DIR` (default `~/.local/bin`). From the release after 0.7.0, a `DESTDIR` set for a package build stages that file under `$DESTDIR$PIXEL_INSTALL_DIR` instead; the 0.7.0 script ignores `DESTDIR`. It edits no shell profile.

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

To build from source, see [CONTRIBUTING.md](https://github.com/Pixel-CLI/pixel/blob/main/.github/CONTRIBUTING.md).

Then let your agents use it, and check the result:

```bash
pixel setup         # once, from anywhere
pixel prepare-repo .  # optional: index now; otherwise your first code prompt starts it
pixel doctor .        # optional: health check
pixel list-signatures path/to/a/large/file   # first result: full read vs Pixel, in tokens
```

The index, the code graph and the optional history data live in `.pixel/` at the repository root and stay on your machine, and there is no telemetry beyond an optional release check. The network is used only for Git remote operations, plugins you install, the optional `pixel classify` (a core command) and `pixel web-search` (a command of the `pixel-utilities` plugin), the embedding model downloaded from Hugging Face on first use, and a release check throttled to at most once per day when a person is at a terminal (`PIXEL_NO_UPDATE_CHECK=1` turns it off) ([security model](https://github.com/Pixel-CLI/pixel/blob/main/.github/SECURITY.md)).

## What pixel setup wires

`pixel setup` is global: run it once, from anywhere. In a terminal it asks which agents and features to switch on and lists every file before it writes; with `--yes`, in CI, on a pipe or with `--json` it never asks. For each agent it writes up to two things: a hook that adds a source-cited `[PIXEL:BRIEF]` (definitions, callers, code, tests and Git history, cited `file:line`) to each code prompt before the model answers, and the `pixel` skill that maps questions to commands. Machine-wide it adds `~/.pixel/config.yaml` and the first-party brief probes in `~/.pixel/plugins/`. No prompt, rules file or retrieval guard is written: every agent keeps its native search. `PIXEL_BRIEF=0` turns the briefs off.

{{% agents-install %}}

Each agent's page under [For your agent](../for/) names the files, the check and the removal, including the agents `pixel setup` writes nothing for. Restart the agent after `pixel setup`; Codex also asks you to review the new hooks with `/hooks`.

Indexing needs no command: a code prompt in a repository with no index starts `pixel prepare-repo` in the background, and the next prompt is briefed. Run `pixel brief "<your question>" .` to see what your agent receives.

`pixel setup --remove` removes everything `pixel setup` wrote, and the binary at `~/.local/bin/pixel`, where the install script puts it; `--dry-run` lists it first. A package manager removes its own copy: uninstall with the manager that owns it (`mise uninstall pixel`, or the Homebrew equivalent).

### Per repository

`pixel setup --repo <path>` skips every global step and creates `<repo>/.pixel/config.yaml`, a commented template for the settings that may differ in that repository; the global file is `~/.pixel/config.yaml`, and `pixel config` shows each effective value and the file it comes from. Pixel registers no hook or guard inside a repository, so nothing else is written under it.

Retrieval stays native: nothing Pixel registers answers, rewrites or denies a search or a read. [Pi integration](https://github.com/Pixel-CLI/pixel/blob/main/docs/pi-harness.md) describes the `/pixel-impact` package. `pixel doctor <repo>` checks the install and this repository's index.

## Updating

Upgrading replaces the binary only. The per-agent config keys belong to you, not to the package manager, so they keep the old release's text until you refresh them.

| Installed with | Upgrade the binary |
| --- | --- |
| mise | `mise upgrade pixel` |
| `install.sh` | run the same `curl … \| sh` line again |
| Source checkout | `pixel self-update` rebuilds and reinstalls the running binary |

Then, whatever the channel:

```bash
pixel setup
pixel doctor . --fix   # runs each repair a flagged check names, then re-checks
```

`pixel doctor .` reports the wiring as stale until you do, with the command that repairs each finding, and exits 1 while a check is red.

## Command plugins

Commands move out of the binary into plugins, one repository each under `Pixel-CLI`: `pixel-git` (`commit`, `push`, `plan-rollback`, `squash-branch`, `fast-forward` and the other git commands), `pixel-flow` (`flow`, `ultraflow`), `pixel-recall` (`recall`), `pixel-telemetry` (`token-savings`, `action-log`, `list-errors`) and `pixel-utilities` (`web-search`, `check-release`, `ai-cli-readify`). The binary builds in none of these except `check-release`, which stays (the release workflow runs it before any plugin could be installed): without the plugin installed, `pixel <name>` prints how to get it (`pixel install github:Pixel-CLI/pixel-flow`) and exits 2. The suggestions pixel prints name a plugin command only where that plugin runs. A plugin that declares network access (`pixel-git` does, for `push`, `fetch` and `sync-branch`) runs after `pixel plugin enable git`. Every plugin run is written to the action log like a core command. `pixel install <source>` takes `github:<owner>/<repo>[@<ref>]`, a git URL or a local directory; a program the repository does not ship comes from the release asset `pixel-<name>-<target-triple>` (checked against its `.sha256` when published), otherwise `pixel install` prints the `cargo install --git` command to build it and the plugin stays not runnable until it is. `npm:<package>` is not supported yet. `pixel plugin list` shows what is installed, `pixel uninstall <plugin>` removes one.

## The workflow

Every agent keeps its native search and editing, and a code prompt already carries the brief. Choose Pixel commands when their facts help with a specific task:

```bash
pixel scope-task "<task>"        # optional task scope and candidate targets
pixel find-code "<phrase>"       # optional lookup by behavior
pixel impact "<symbol>" --no-refresh # explicit impact query using an existing fresh graph
pixel what-changed               # inspect what already differs
pixel review-changes             # the working tree, structured (pixel-git plugin)
pixel commit-and-push --files <f1> --files <f2> -m "msg" --request-id "id" origin HEAD   # pixel-git plugin
```

An impact query is optional and requires a known symbol. Inspect its cited source and continue with native tools when the graph is unavailable, stale, ambiguous or unhelpful. The agent commits or pushes only when asked; commit operations use a `--request-id` for idempotent execution when the journal record remains: terminal records are retained for at most 1,000 entries and up to 30 days, a pruned request ID can start a new operation, and recovery may return an error when a crash leaves the commit or push outcome ambiguous.

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

These commands are the `pixel-git` [command plugin](#command-plugins) (`pixel install github:Pixel-CLI/pixel-git`).

| Instead of | Run |
| --- | --- |
| `git log -S "x"` | `pixel dig-history --phrase "x"` |
| `git log --grep "x"` | `pixel search-history "x"` |
| `git log --follow f` | `pixel file-history --file f` |
| "it worked before" | `pixel plan-rollback "<problem>"` (a [command plugin](#command-plugins)): flags the breaking commit, writes nothing without `--apply` |

### Git changes

Also the `pixel-git` plugin.

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

`pixel recall` (the `pixel-recall` [command plugin](#command-plugins)) searches the transcripts of every agent on the machine (Claude Code, Codex, Pi and others). `pixel recall search "token"` finds an exact string, `pixel recall ask "topic"` a topic in your own words, and `pixel recall show <ref> --turn N..M` reads the turns around a hit.

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

Pixel does not cover every job. Use the native command for grep flags Pixel lacks (`-m`, for one), pipelines, files outside the index (git-ignored, binary, or over 4 MiB), in-place edits with `sed`, interactive Git such as `rebase -i` and `stash`, and network operations such as `clone`.

## Token savings

`pixel token-savings` (a [command plugin](#command-plugins)) reports, for the retrieval commands you ran, the fraction of the candidate pool the agent did not have to read. It measures what reached the agent's context, not your invoice. The replay of [shunt](https://github.com/spotify/portal-ai-plugins/tree/main/plugins/shunt)'s benchmark on Pixel's own repository is on the [benchmarks page](../benchmarks/#reading-code), with its method.

Each Pixel command also prints a `🟩 Pixel` line on stderr with its measured duration and two estimates: tokens saved against the native workflow, and time saved against sequential round trips. Both are estimates, and zero or negative values are valid. `--metrics=off` or `PIXEL_METRICS=0` turns the line off.

One command measures instead of estimating: `pixel list-signatures <file>` stands in for reading that file, so its line compares the file with the outline it printed, as `full read 10365 tok, pixel answer 641 tok (-94%)` (Requests' `models.py`). Both counts are bytes divided by four, rounded down, the method of the [benchmarks page](../benchmarks/#well-known-files), and it works on a fresh clone with no session behind it.
