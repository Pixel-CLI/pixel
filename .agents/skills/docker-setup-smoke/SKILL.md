---
name: docker-setup-smoke
description: Replay Pixel's new-user setup in Docker without an LLM — install through a release archive, install.sh, the Homebrew tap, or sources from main, a PR head or a pinned commit; then personal Claude/Codex/pi settings kept, idempotent reinstall, first pixel audit, doctor and --fix, project install, disabled classify and uninstall; with --agents, Claude Code, Codex and pi sessions against a scripted fake model show whether each receives the Pixel prompt, runs pixel and routes grep. Use for Linux setup smoke checks, not macOS behavior or real model inference.
---

# Docker setup smoke

Run a Linux binary with a fresh non-root user, pre-existing personal agent
settings and a small Git repository. Docker Desktop on macOS works; the tests
still exercise Linux. No Docker Agentic Platform or model key is needed.

From the repository root:

```bash
sh .agents/skills/docker-setup-smoke/scripts/run.sh
# Another published release archive:
sh .agents/skills/docker-setup-smoke/scripts/run.sh v0.6.1
# The published install.sh, into a dedicated directory:
sh .agents/skills/docker-setup-smoke/scripts/run.sh --installer
# The Homebrew tap on Linuxbrew:
sh .agents/skills/docker-setup-smoke/scripts/run.sh --brew
# Compile current upstream main:
sh .agents/skills/docker-setup-smoke/scripts/run.sh --source main
# Compile a PR's head, including a fork PR (not GitHub's merge ref):
sh .agents/skills/docker-setup-smoke/scripts/run.sh --pr 427
# Replay the exact source SHA recorded by a prior run:
sh .agents/skills/docker-setup-smoke/scripts/run.sh --source <40-character-SHA>
# Any of the above, then agent sessions against the fake model:
sh .agents/skills/docker-setup-smoke/scripts/run.sh --agents --source main
```

## Binary modes

- **Release archive** (default `v0.6.1`): download that exact release's
  architecture-specific archive and verify its published SHA-256.
- **`--installer`**: fetch `releases/latest/download/install.sh` and run it as
  the test user with `PIXEL_INSTALL_DIR=$HOME/opt/pixel/bin`. The installer
  always resolves the latest release, so this mode tests whatever that is on the
  day; `binary-version.txt` and `installer.log` record which one.
- **`--brew`**: `brew install LivioGama/tap/pixel` in the pinned `homebrew/brew`
  image, latest formula. Homebrew's prefix is private to its `linuxbrew` user, so
  that user runs the checks; uninstall must leave the Cellar binary to Homebrew.
  `brew-deps.txt` lists what the formula pulled in, and `brew-host.txt` whether
  Homebrew judges the host's glibc or libstdc++ older than its CI's: then it gives
  every formula, bottled or not, an implicit `gcc` and `glibc`. A control formula
  with no dependency (`local/smoke/control`) measures what the host adds
  (`brew-host-deps.txt`); a dependency of pixel beyond it fails the run. The image
  (Ubuntu 22.04, glibc 2.35) is such a host: its 12 are the control's, and a `NOTE`
  says so.
- **Source** (`--source`, `--pr`): fetch from `https://github.com/Pixel-CLI/pixel.git`
  inside the container, check out the fetched commit detached, and build with
  pinned Rust 1.98.1, `--locked --no-default-features --features model2vec`, debug
  profile without debug info: this tests setup contracts, not release performance
  or musl compatibility. The checkout and build directory are discarded; each run
  is a cold build of several minutes. `pixel --version` must report the fetched SHA.

## What `checks.sh` asserts

1. Personal settings exist before Pixel: `~/.claude/settings.json` (model,
   permissions, a `PreToolUse` Edit|Write hook and a `SessionStart` hook),
   `~/.claude/CLAUDE.md`, `~/.codex/config.toml`, `~/.codex/hooks.json` with a
   user hook, `~/.codex/AGENTS.md`, `~/.pi/agent/{settings.json,AGENTS.md,APPEND_SYSTEM.md}`.
2. Global install keeps every personal key, hook and instruction line.
3. A second global install leaves every managed file byte-identical (backups
   excluded).
4. Metrics configuration persists without an env override.
5. First `pixel audit` in a never-indexed repository prints
   `no code graph yet, building it (first run only)` on stderr, reports coverage
   (`indexed: python 4/4`) and file counts; the second run does not rebuild.
6. `pixel doctor --fail-on yellow` fails in a never-prepared repository, every
   `[red]` line is followed by a `fix:` line, and after `--fix` no check is red.
   The only yellow allowed then is `install.codex-hook-review` /
   `repo.codex-hook-review` naming `/hooks`: Codex runs a hook only after the
   user reviews it, which no command can do for them.
7. Project install keeps `AGENTS.md` user instructions and is repeatable;
   disabled classify fails with empty stdout and the disabled diagnostic;
   project uninstall restores `AGENTS.md` byte for byte.
8. Global uninstall restores the personal JSON files to equal values and the
   text files byte for byte, removes the prompt and a binary pixel owns (not a
   Homebrew one), and a second uninstall succeeds.
9. Files left behind that the user did not have before are listed in
   `residue.txt` and counted in a `NOTE` line, not failed. Its `*.pixel-bak.*`
   files must be exactly those the last uninstall's `backups` step names in its
   `rm --` command (read back with `shlex`), and the only other file
   `~/.pixel/config.yaml`, which `pixel config --global` wrote; a release
   without that step gets a `NOTE` instead.

A personal `PreToolUse` hook matching Bash makes `pixel install --repo` skip the
Claude guard (yellow `claude guard not installed`); the fixture matches
Edit|Write so the guard is installed. The checks add such a hook for one extra
project copy (then restore `settings.json` byte for byte): the install step must
name the hook and what to do (narrow its matcher, or work without the guard),
and `repo.claude-hooks` must be yellow with the same advice and no `fix:` line;
a release without that advice gets a `NOTE`.

## Agent sessions (`--agents`)

The image then also carries Node and pinned agent CLIs (`node_version` and
`agent_packages` in `run.sh`: Node v24.21.0, Claude Code 2.1.285, Codex 0.159.2,
pi 0.99.1). After `checks.sh`, `agents.sh` reinstalls Pixel over the restored
personal settings, installs it in a copy of the project, and runs one
non-interactive session per agent: `claude -p`, `codex exec`, `pi --print`.

No model or key is involved. `fake-llm.py` listens on `127.0.0.1:8765` inside the
container and speaks Anthropic Messages (Claude Code, and pi through a
`models.json` provider) and OpenAI Responses (Codex through `-c
model_providers.smoke=…`). It plays a fixed script: call the agent's shell tool
with `pixel search-content -F helper_1 src`, then with `grep -rn helper_1 src`,
then answer `FAKE_LLM_DONE`. Every request is logged to
`<agent>-requests.jsonl`, which is the evidence of what the agent sent a model.

The project is trusted the way a user accepting the prompt would: a Codex
`projects."<path>".trust_level="trusted"` override and pi `--approve`. Without
it, neither loads project-level configuration.

Asserted per agent: exit and final answer, the Pixel prompt in the first model
request (`deterministic repository facts`, the agent prompt's heading, for Claude's SessionStart context,
`pixel:managed:begin` for Codex developer instructions and pi's
`APPEND_SYSTEM.md`), both tool results fed back, and a new `search-content` row
in the project's `.pixel/actions.jsonl` from the model's pixel call. Reported,
not asserted:

- Claude: whether the guard routed the native `grep` (a new `search-compat`
  row), and whether the prompt arrived only as a `<persisted-output>` preview
  because the hook output exceeded Claude Code's 10 000-character inline limit.
- Codex: how many Pixel hooks ran (`run-hook … --provider codex` rows), then a
  second session with `--dangerously-bypass-hook-trust` standing in for the
  user's `/hooks` review, which tells an unreviewed hook from a broken one.
- pi: the guard's last `bash` decision in `.pixel/pi-policy.jsonl` under the
  default policy, then again under `pixel config policy enforce`. pi's guard
  blocks rather than reroutes, so it never writes `search-compat` rows.

A scripted model shows the harness wiring — prompt delivery, hooks, guards, the
binary on the agent's PATH — not whether a real model follows the prompt.

## Environment and isolation

`scripts/Dockerfile` holds the environment only: the pinned base image, curl,
Git, Python and the `tester` user. The runner builds it without a build context
as `pixel-setup-smoke:<mode>`; Docker's layer cache skips the apt step after the
first run. Fetching or compiling pixel and the checks stay in `docker run`, so a
failed step still leaves a container to export evidence from and a moving ref
such as `main` is never served from a cache. The cached apt layer keeps the
packages of its first build: remove the image (`docker image rm
pixel-setup-smoke:<mode>`) to refresh them or reclaim space.

Only this skill's scripts are mounted, read-only. Host home, project, credentials
and Docker socket are not mounted. The runner removes its own container on
completion or interruption, never prunes other resources, and leaves the base
and smoke images cached.

## Evidence and limits

The runner prints a directory under `target/docker-setup-smoke/` containing the
checkout SHA, Docker version, base image digest, built image ID, test user,
invocation, binary provenance, image build log, complete run log and exit status,
plus every JSON report, audit and doctor output, the personal-settings snapshot
and the residue list, exported before container removal, including on failure.
Report the tested binary's version and commit separately from the runner checkout
SHA. Source mode tests fetched upstream code, not uncommitted local changes.

Not covered: macOS (Homebrew on macOS included), shells other than bash,
interactive setup and agent TUIs, real model inference or a model's compliance.

Prerequisites: a POSIX sh, Git, a running Docker engine with BuildKit (Docker Desktop's
default) and network access to GitHub, Docker Hub and crates.io, plus Debian or
Ubuntu mirrors until the smoke image is cached. The image build is not
time-bounded; bootstrap is bounded to 5 minutes (release, installer), 15 minutes
(brew) or 30 minutes (source), checks to 5 minutes and agent sessions to 10.
`--agents` also needs nodejs.org and the npm registry until its image is cached.
CPU/memory limits are 4 CPUs/6 GiB.
On failure inspect the saved evidence and fix the cause before replaying.

When changing the runner, run `python3 scripts/test-runner.py` from this skill
directory and ShellCheck on its shell scripts. The contract tests verify mode
selection, invalid-selector rejection, failed-build and failed-run evidence and
the fake model's script in both wire formats without Docker; also replay the lifecycle in Docker for the modes affected by the
change.
