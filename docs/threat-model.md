# Pixel threat model

This document is Pixel's threat model and attack surface analysis (OpenSSF
Baseline OSPS-SA-03.02). It names what an attacker could want from Pixel,
who can reach which entry point, what the code does about each threat today,
and what is left. It was written from the source on `main` after v0.6.1
(2026-10); every mitigation below names the function, constant, path or
workflow that implements it, as the code spells it, so a reader can check it.

[SECURITY.md](../SECURITY.md) is the short policy: supported versions, how to
report a vulnerability, how to verify a release. [ARCHITECTURE.md](../ARCHITECTURE.md)
is the map of crates, on-disk state and the daemon wire contract this document
builds on. [assurance-case.md](assurance-case.md) builds on this document in
turn: the security claims, the design principles and the common weaknesses
it counters. A suspected vulnerability goes to the private advisory link in
SECURITY.md, never to an issue or a pull request against this file.

Statuses used below:

- **Mitigated**: a control in the code addresses the scenario as described.
- **Partial**: a control exists but leaves a named gap.
- **Accepted**: no control, by design or because the trust boundary is
  elsewhere; the residual risk is stated.

## 1. Scope and assets

Pixel is a local CLI (`crates/pixel`, binary `pixel`), a per-repository
daemon (`pixel-daemon`) reached over a Unix socket, hooks it installs into
coding agents (`pixel-install`), and the release chain that ships the binary.
Out of scope: the agent hosts themselves (Claude Code, Codex, Pi, …), the
model providers, and the website under `website/`.

| Asset | Where | Why it matters |
| --- | --- | --- |
| Source and history of indexed repositories | the working tree, read through `pixel-git::GitRunner` and the walkers in `pixel-index`/`pixel-graph` | confidentiality of code the user did not mean to share; integrity of what the agent is told about it |
| Per-repository index and sidecars | `.pixel/` (ARCHITECTURE.md, "On-disk state"): `base.shard`, `delta.shard`, `graph.v2.db` (`pixel_daemon::api::GRAPH_DB_FILE`), `history.db`, `code-vectors/`, `targets.json`, `actions.jsonl`, `config.yaml`, `tasks/`, `env-snapshots/` | what the agent reads as ground truth; `actions.jsonl` and `env-snapshots/` can hold secrets |
| Machine-wide state | `~/.pixel/config.yaml` (remote keys), `~/.local/share/pixel/flows/` (fill values: passwords, OTPs), `~/.local/share/pixel/recall/` (agent transcripts), `~/.local/share/pixel/models/`, `~/.local/state/pixel/` (`pixel-ops` journals, snapshots, locks; the `pixel-session` error sink) | secrets at rest, and transcripts that quote them |
| Daemon socket | `pixel_daemon::daemon::socket_path`: `$TMPDIR` on macOS, `$XDG_RUNTIME_DIR` or `~/.cache/pixel/sockets/` on Linux | any client of the socket can ask for git mutations on the repository |
| Agent configurations | what `pixel install` writes: `~/.claude/settings.json`, `$CODEX_HOME/config.toml` and `hooks.json`, `~/.pi/agent/APPEND_SYSTEM.md`, OpenCode, Antigravity, zcode and Devin configs; per repository with `--repo`, `.claude/settings.local.json`, `.codex/`, `.devin/config.local.json`, `.pi/extensions/pixel-guard.ts`, the managed block in `AGENTS.md` | a hook command runs with the user's privileges on every agent tool call |
| User secrets | provider keys (`OPENROUTER_API_KEY`, `OLLAMA_API_KEY`, `DEEPSEEK_API_KEY`, `OPENCODE_API_KEY`, `PERPLEXITY_API_KEY` or `remote_keys` in the global config), `.env` values edited by `pixel edit-env` | credential theft, billing abuse |
| Release chain | tags `v*`, `.github/workflows/release.yml` and `release-build.yml`, the `HOMEBREW_TAP_TOKEN` and `VT_API_KEY` secrets, the build-provenance attestation, `scripts/install.sh`, the Homebrew tap | a tampered release runs on every user's machine |
| CI | `.github/workflows/*.yml`, the `PROJECTS_TOKEN` secret, the self-hosted runner named by the `PIXEL_RUNNER_LABELS` repository variable | a foothold in CI is a step towards the release chain |

## 2. Actors and trust boundaries

| Actor | Trust | Reaches Pixel through |
| --- | --- | --- |
| The user | trusted: Pixel acts with the user's privileges | the CLI, `pixel install`, the global config |
| The coding agent and the model driving it | semi-trusted: acts with the user's privileges, and can be steered by prompt injection from anything it reads, including Pixel's own output | CLI arguments, hook payloads, the daemon socket |
| Repository content | untrusted when the repository is not the user's own: files, commit messages, branch names, a committed `.pixel/` or `.codex/`, git configuration that came with an archive | the walkers, tree-sitter, git, every sidecar reader |
| Other processes of the same user | trusted by the operating system; not distinguished by Pixel | the daemon socket, every file Pixel writes |
| Other local users | untrusted | file and socket permissions only |
| Network services | untrusted for integrity, trusted with what is sent to them | Hugging Face, the classify endpoints, the web-search endpoints, `github.com` release checks, `ollaya.dev` |
| Contributors | untrusted until review | pull requests, which CI builds and tests |
| Maintainers | trusted ([GOVERNANCE.md](../GOVERNANCE.md)) | merges, tags, repository secrets and settings |

The boundaries a change can cross, referred to below as B1 to B7:

```text
 hostile repo ──B1──▶ pixel (parse, index, git) ──B3──▶ agent context (prompt)
 agent/model  ──B2──▶ pixel CLI / hooks
 same-user process ──B4──▶ daemon socket ──▶ git mutations on the repository
 pixel install ──B5──▶ agent configs (hooks run on every tool call)
 pixel ◀──B6──▶ network (models, classify, web search, release check)
 contributor ──B7──▶ CI ──▶ release ──▶ every user's machine
```

## 3. Attack surface by entry point

### 3.1 CLI arguments (B2)

clap parses argv in `crates/pixel/src/main.rs`; the agent chooses most
arguments. Paths and patterns are passed to library code, never to a shell.
Outside the hooks (3.3), two paths run a shell on purpose: `pixel self-update
--build "<cmd>"` (`sh -c`, default `cargo build --release -p pixel-cli`) and
the update prompt's `update_notice::run_upgrade` (`sh -c` on the command
`update_notice::upgrade_hint` builds); `pixel config edit` starts
`$VISUAL`/`$EDITOR`. Each invocation is recorded in `.pixel/actions.jsonl`
through `logged_args` (`crates/pixel/src/main.rs`). Stdout is capped at
256 KB (`PIXEL_OUTPUT_CAP_BYTES`).

### 3.2 Daemon socket and protocol (B4)

One NDJSON `Op` per line in, one `Envelope` out (ARCHITECTURE.md, "Daemon and
wire contract"). In `crates/pixel-daemon/src/daemon.rs`: `MAX_REQUEST_LINE`
(64 KiB), `CONNECTION_DEADLINE` (5 s per connection), a 1 s read timeout,
`MAX_REQUESTS_PER_CONN` (64), `IDLE_TIMEOUT` (30 min). The socket file is
set to 0600 after `bind`; on Linux the fallback directory is set to 0700.
`PROTOCOL_VERSION` (`pixel_daemon::api`, 13) is compared by the CLI's
`classify_ping`: an older daemon is shut down and replaced, a newer one is
left alone and the command runs in process. Requests are served one at a
time. The op set includes git mutations (`publish`, `push`, `ship`,
`branch_op`, `update`, `sync`, `reconcile`), file writes (`rename`), index
rebuilds and `shutdown`. The CLI starts a daemon on demand
(`auto_start_daemon`; `PIXEL_DAEMON_AUTO_START=0` turns that off).

### 3.3 Installed hooks (B2, B3, B5)

`pixel run-hook <entry>` reads one JSON payload from the agent host on stdin
(ARCHITECTURE.md, hook table). The payload's `cwd`, `tool_input` and session
fields are agent-controlled. Entry points:

- `guard` can rewrite native search and read commands into Pixel commands
  for supported providers. Codex and Claude preserve native retrieval commands
  even under the shared enforce policy. Their standard installation registers
  no automatic retrieval prompt or metrics callback.
  `search_compat::shell_argv` accepts a small grammar only (it refuses `$`,
  backticks, `\`, newlines, and unquoted operators, globs and `~`), every
  rewritten word is re-quoted with `search_compat::shell_quote`, and an
  unsupported command stays native. For Devin and zcode `PermissionRequest`
  events, `guard::retrieval_permission_response` approves a closed set of
  read-only Pixel commands without asking the user.
- The legacy/manual `composed-guard` replays a sealed copy of a repository's pre-existing Codex
  `PreToolUse` hooks (`.codex/pixel-composed-guard-backup.json`) through
  `/bin/sh -c` (`guard::run_foreign_command`, 2 s `COMPOSED_TIMEOUT`). Standard
  installation restores the original registrations and removes this wrapper.
- `session-start`, `prompt-submit`, `post-compaction`, `post-tool-use` and
  `metrics` add text to the model's context.
- `task-event` gates edits and completion for a task contract.

### 3.4 Repository content: walking, parsing, indexing (B1)

`pixel-index` walks the tree with `ignore::WalkBuilder` (links not followed)
and opens files through `pixel_index::index::open_regular_bounded`, which
refuses a final-component symlink and anything over `MAX_FILE_BYTES`
(4 MiB). `pixel-graph` parses each file with tree-sitter
(`pixel_graph::extract::extract_file`) under the same 4 MiB cap, cancels any
parse still running after `PARSE_BUDGET` (3 s; the file then contributes no
rows), skips files with a NUL in their first `BINARY_SNIFF_BYTES`, and stops at
`DEFAULT_GRAPH_MAX_FILES` (50 000). Git-anchored shards read committed blobs
through `git cat-file` rather than the filesystem.

### 3.5 Sidecar files under `.pixel/` (B1)

Pixel reads back what it wrote: shards (`pixel_index::shard::Shard::open`,
bounds-checked against `MAX_FILE_COUNT`, `MAX_GRAM_COUNT`, `MAX_PATH_LEN`
before use), SQLite databases, JSON manifests. `.pixel/` is listed in the
clone's `info/exclude` (`pixel_index::lock::ensure_pixel_gitignored`), which
keeps Pixel's own files out of commits but does not stop a repository from
shipping a tracked `.pixel/`. Since 0.7.0 (GHSA-c9f5-vxc4-wjph), the stores
refuse such a directory before reading it (`pixel_git::sidecar::check`), files
under it are written without following links (`pixel_git::nofollow`,
`SQLITE_OPEN_NOFOLLOW`), and a path read from a shard or the graph is used only
inside the repository (`pixel_git::repo_path::confine`); see T6.

### 3.6 Git operations (B1, B2)

`pixel-git::GitRunner` is the only git spawner
(`crates/pixel-git/tests/boundary.rs` fails the build on a
`Command::new("git")` anywhere else). `DEFAULT_TIMEOUT` is 120 s,
`DEFAULT_MAX_OUTPUT_BYTES` 1 MiB, and stderr passes through
`pixel_git::redact::redact`. Caller-supplied refs and remotes go through
`pixel_git::validate_ref` (`ref_guard.rs`: no leading `-`, a closed character
set) and `--end-of-options`; pathspecs follow `--`; commit messages are one
`-m` argument. `GitRunner::run_isolated`, which also clears `GIT_*`, points
the global and system configs at `/dev/null` and disables hooks and
`core.fsmonitor`, is used for the task layer's private snapshots
(`pixel-task/src/snapshot.rs`); every other git call runs with the
repository's own configuration and hooks.

### 3.7 Configuration (B1, B2)

Settings resolve environment, then the repository's `.pixel/config.yaml`,
then the global `~/.pixel/config.yaml` (`config_cmd::layers`,
`policy_resolution`). The repository layer can set `policy`,
`daemon_auto_start`, `metrics`, the task context switches and the `task:`
section, including `task.checks`, argv lists that `pixel task-state verify`
runs ([docs/task-optimizer.md](task-optimizer.md)). Classify, web search and
keys are global only: `config_cmd::remote_key`, `classify_engine`,
`classify_remote_preset`, `ollaya_launch`, `web_search_searxng_url` and
`web_search_perplexity_key` read `global_config_path()` and never the
repository file.

### 3.8 Network (B6)

- `pixel classify --engine remote`: the presets in
  `crates/pixel/src/decide_remote.rs` (`Preset::base`), over `ureq` with
  `rustls-webpki-roots`; the question and its context are sent, each capped
  at `TEXT_CAP_CHARS` (32 768). `PIXEL_REMOTE_BASE` overrides the endpoint.
- The prompt hook's task-intent question, when classify is enabled
  (`prompt_intent::hook_intent`, `HOOK_CALL_TIMEOUT` 300 ms), carries the full
  prompt to the Ollaya endpoint the global config names
  (`classify_setup::local_base`: the launch entry's `base`, else
  `decide_ollaya::DEFAULT_BASE` on loopback). A non-loopback base receives
  every prompt the hook classifies.
- `pixel web-search`: SearXNG at `PIXEL_WEB_SEARCH_URL`, Perplexity with a
  key, else DuckDuckGo then Wikipedia (`crates/pixel/src/web_search.rs`).
- The release check: one `HEAD` to `RELEASES_LATEST_URL`
  (`update_notice.rs`), off with `PIXEL_NO_UPDATE_CHECK=1`, never for hooks or
  under `CI`.
- Model downloads: `minishlab/potion-*` (`pixel_recall::embed::POTION_REPO`)
  and, with the `fastembed` feature, `intfloat/multilingual-e5-small`, from
  Hugging Face into `~/.local/share/pixel/models/`.
- `pixel index-unpack <path|url>` (`crates/pixel/src/index_cmd.rs`,
  `FETCH_CAP` 2 GiB).
- The local engine setup (`classify_setup::setup_local_with`) downloads and
  runs `https://ollaya.dev/install.sh`.

### 3.9 Install, update, uninstall (B5, B7)

`pixel install` edits agent configs in place, rewriting only its own managed
blocks and hook entries, refusing files that do not parse, and leaving a
`<file>.pixel-bak.<nanos>-<seq>` copy of each file it changes
(`pixel_install::config::backup_if_changing`). Hook commands embed the
absolute binary path, quoted by `routing::quoted_executable`. The binary
reaches users through `scripts/install.sh` (archive checked against the
release's `.sha256`), Homebrew, mise, or the `setup-pixel` action (exact
version, sha256 check).

### 3.10 CI and release (B7)

Pull requests run `ci.yml`, `cross-build.yml`, `codeql.yml`,
`fuzz.yml` and the others listed in ARCHITECTURE.md, "Testing and gates".
`board-sync.yml` runs on `pull_request_target` to move the project board.
`mutants.yml` runs the cumulative main diff nightly, with read-only contents
and Actions metadata access. It accepts checkpoint metadata only from its
own completed main runs, and validates checkpoint ancestry before selecting
the diff. Only fully judged campaigns write checkpoint artifacts; only scheduled main runs can produce or supply them.
`coverage.yml` runs only at 02:47 UTC on main, with read-only contents and
Actions access. Both instrumented jobs are skipped only after a successful
scheduled measurement of the same SHA. Coverage is post-merge feedback;
normal tests and lint still validate the PR head before merge. Local checks,
including pre-push validation and reinstalling, are optional diagnostics.
A `v*` tag runs `release.yml`, which calls `release-build.yml` to build and
sign on GitHub-hosted runners.

## 4. Threats

Each threat names its STRIDE category (Spoofing, Tampering, Repudiation,
Information disclosure, Denial of service, Elevation of privilege) and the
boundary it crosses.

### T1. A same-user process drives the daemon (E, B4)

- **Scenario**: any process running as the user, including an agent sandboxed
  from the network or from `git push` but still able to `connect()` to the
  socket, sends `publish`, `push`, `reconcile`, `rename` or `shutdown`. The
  daemon runs them with its own environment, which it inherited from the
  first CLI call that started it.
- **Mitigation**: the socket file is 0600 and lives in a per-user directory
  (3.2). Each daemon serves the one root it was started on.
- **Status**: Accepted (SECURITY.md, "Known limitations").
- **Residual**: there is no peer-credential check and no per-op
  authorization, so an agent sandbox that leaves the socket reachable does
  not contain the git mutations. A sandbox that should hold must deny the
  socket path or run with `PIXEL_DAEMON_AUTO_START=0` and no daemon.

### T2. Another local user reaches the socket (S, I, B4)

- **Scenario**: a second account connects to the socket, or pre-creates the
  socket directory or path so the CLI talks to its listener.
- **Mitigation**: macOS `$TMPDIR` is per user; on Linux
  `$XDG_RUNTIME_DIR` is per user by systemd convention, and the fallback
  `~/.cache/pixel/sockets/` is set to 0700 (`daemon::runtime_dir`); the
  socket is set to 0600 after `bind`.
- **Status**: Partial.
- **Residual**: neither the directory's owner nor its mode is verified, the
  0600 mode is applied after `bind` (the directory covers that window), and
  the CLI does not check who owns the socket it connects to. A runtime
  directory shared between users (a `TMPDIR` set to `/tmp`) breaks the
  model.

### T3. Daemon denial of service (D, B4)

- **Scenario**: a client stalls the single-threaded request loop.
- **Mitigation**: `MAX_REQUEST_LINE`, `CONNECTION_DEADLINE`, the read
  timeout and `MAX_REQUESTS_PER_CONN` bound what one connection can send;
  `IDLE_TIMEOUT` ends an unused daemon; the CLI falls back to in-process
  execution when the daemon does not answer.
- **Status**: Partial.
- **Residual**: there is no write timeout on replies, and long ops (a sync of
  several 120 s git calls) block every other client of that root. Only a
  same-user process can do this (T1).

### T4. Version skew between CLI and daemon (T, B4)

- **Scenario**: an old daemon serves a newer CLI with a request format it
  misreads.
- **Mitigation**: `classify_ping` compares `PROTOCOL_VERSION`; an older
  daemon gets `Shutdown` and is replaced; `pixel_proto::Op` has a unit test
  per variant (`op_name_matches_serde_tag`,
  `session_capabilities_track_every_real_op`).
- **Status**: Mitigated.
- **Residual**: `Envelope::validate` (`ENVELOPE_PROTOCOL_VERSION`) runs as a
  `debug_assert!` only.

### T5. Hostile source files crash or exhaust the parsers (D, B1)

- **Scenario**: a repository ships a huge, binary, deeply nested or malformed
  file to crash `pixel build-index` or the graph build, or to make a search
  pattern miss.
- **Mitigation**: the caps in 3.4, including the per-parse `PARSE_BUDGET`
  that cuts off tree-sitter error recovery a few hundred malformed bytes can
  stretch to minutes (#800); symlinks are not followed by the walk;
  `Shard::open` checks every section length with checked arithmetic before
  indexing into the map; the `graph_extract` fuzz target feeds arbitrary
  source to `extract_file` and `extract_concepts`, and `search_plan` checks that the query planner
  never drops a document the verifier matches (`fuzz/`).
- **Status**: Partial.
- **Residual**: the tree-sitter grammars are C code outside Rust's memory
  safety, covered by fuzzing of one entry point only; `Shard::open` maps the
  file (`unsafe { Mmap::map }`), so a shard truncated while mapped can raise
  `SIGBUS`.

### T6. A repository ships its own `.pixel/` (T, I, B1)

- **Scenario**: the repository tracks files under `.pixel/` (a `git add -f`),
  so the first `pixel` command in the clone finds an index, a graph, a
  history database, task manifests or a configuration it did not build.
- **Mitigation** (0.7.0, GHSA-c9f5-vxc4-wjph): before the index, graph and
  history stores read anything, and before `pixel index-unpack` installs,
  `pixel_git::sidecar::check` refuses a `.pixel` that is a symbolic link or
  holds files git tracks, and names the command that removes it. Files under
  `.pixel/` are opened with `O_NOFOLLOW` or created fresh and renamed into
  place, permissions are set on the open descriptor, and the SQLite databases
  open with `SQLITE_OPEN_NOFOLLOW` (`pixel_git::nofollow`), so nothing is
  written, truncated or chmodded through a link. A path read from a shard or
  the graph is used only when it is a plain relative path whose directory
  resolves inside the repository (`pixel_git::repo_path::confine`), and the
  credential filter covers `credentials`, `.netrc` and `.git-credentials`.
  The integrity checks inside the files (the `_pixel_marker` table, extractor
  ids, the graph freshness signature) still only tell current files from stale
  or foreign ones.
- **Status**: Partial.
- **Residual**: the tracked-file check runs when a store opens; the small
  state files other commands and hooks read on each call (the task map,
  repository settings) are not checked against git on every read, so a
  repository can still influence their content, though not write through
  them (SECURITY.md, "Known limitations"). Before running Pixel in a clone
  you do not trust, `git ls-files .pixel` should print nothing.

### T7. Repository git configuration runs code (E, B1)

- **Scenario**: a repository unpacked from an archive or shared on disk
  (not a fresh `git clone`, which carries neither `.git/config` nor hooks)
  sets `core.fsmonitor`, an external diff driver or a filter; Pixel runs
  `git status` or `git diff` for a snapshot, from a hook the user never typed.
- **Mitigation**: none outside the task snapshots, which use
  `GitRunner::run_isolated`.
- **Status**: Accepted: Pixel runs git the way the user's own `git status`
  would, with the same configuration.
- **Residual**: Pixel runs git more often and less visibly than the user
  does, from hooks. Open such a repository only after inspecting
  `.git/config`.

### T8. Argument injection into git (T, E, B2)

- **Scenario**: an agent passes a ref, remote or path that git reads as an
  option (`--upload-pack=…`) or as a different object.
- **Mitigation**: `validate_ref` and `--end-of-options` on caller-supplied
  refs and remotes, `--` before pathspecs, `-m <message>` as one argument,
  the `boundary.rs` test keeping every spawn in `GitRunner`; no git call goes
  through a shell.
- **Status**: Partial.
- **Residual**: pathspec magic (`:/`, `:(glob)`) is not disabled; a remote
  may be a local path (`/abs/repo`), which `validate_ref` accepts; refs read
  from the repository's own state are not all passed through `validate_ref`.

### T9. Repository hooks run on `pixel commit` and `pixel push` (E, B1)

- **Scenario**: `pre-commit`, `commit-msg` or `pre-push` hooks of the
  repository run when the agent publishes.
- **Mitigation**: none; Pixel never passes `--no-verify`, by design.
- **Status**: Accepted: the same hooks run on the user's own `git commit`.
- **Residual**: `GitRunner` kills git on timeout, not its process group, so a
  hook's children can outlive the 120 s timeout.

### T10. A crafted hook payload abuses a hook (T, E, B2)

- **Scenario**: the agent (or text injected into it) shapes the
  `tool_input.command` or `cwd` of a hook payload so that the guard's
  rewrite executes something else, or reads outside the repository.
- **Mitigation**: the hook never executes `tool_input.command`; the rewrite
  grammar and quoting (3.3); rewritten paths must be regular files inside the
  discovered root, outside `.git` and `.pixel`, and not credential-shaped;
  `tool_input.env` disables the rewrite; `guard`, `prompt-submit`,
  `post-tool-use`, `metrics` and `task-event` read their payload through one
  bounded reader, `hook_input::read_bounded`, capped at `MAX_HOOK_INPUT`
  (1 MiB), so an over-cap payload is refused after `cap + 1` bytes instead of
  being allocated in full; `composed-guard` keeps its own
  `COMPOSED_MAX_INPUT` and `post-compaction` its smaller `MANIFEST_MAX_BYTES`
  (64 KiB, which also bounds the manifest file it reads back); over-cap
  `task-event` input emits its unavailable response, which denies `PreToolUse`
  on an enforced session; the other hooks take their existing fail-open exit 0
  and leave the native tool untouched; `pixel install` registers a 10 s
  timeout (`HOOK_TIMEOUT`).
- **Status**: Mitigated.
- **Residual**: the cap bounds allocation, not the wait — a host that writes
  fewer than `MAX_HOOK_INPUT + 1` bytes and holds the pipe open still stalls
  the hook until the host's `HOOK_TIMEOUT` (10 s) ends it; the native fallback
  after a rewrite re-checks the file in Pixel's emulation but not in the
  native `rg`/`grep` it falls back to.

### T11. A hook grants a permission it should not (E, B2)

- **Scenario**: a command chain is approved without the user's prompt because
  the permission parser and the shell disagree on what it does.
- **Mitigation**: `retrieval_permission_response` approves only standalone
  read-only Pixel retrievals (`pixel_spec`'s closed subcommand and flag
  lists), bounded `sed -n 'A,Bp'` reads and stdin-only sinks
  (`is_stdin_sink`); `split_safe_command_chain` refuses `\`, newlines and
  lone `&`; `strip_safe_redirects` keeps only `2>/dev/null` and `2>&1`;
  `word_stays_in_repo` keeps every path inside the root and outside `.git`
  and `.pixel`; `credential_shaped` refuses secret-looking paths; nothing is
  approved when the repository contains `$HOME` (`repo_holds_home`). Tests:
  the `permission_*` tests in `crates/pixel/src/guard.rs`.
- **Status**: Partial.
- **Residual**: a bare `pixel` (and `rtk`) is resolved through the agent's
  `PATH`: a `PATH` that includes a repository-relative directory (a
  direnv-managed `./bin`) lets the repository supply the approved `pixel`.

### T12. Prompt injection through Pixel's output (T, B3)

- **Scenario**: a repository plants instructions in code, comments, commit
  messages, symbol names or file names; Pixel quotes them to the agent, which
  follows them.
- **Mitigation**: the agent prompt states that Pixel output is data, not
  instructions (`crates/pixel-install/assets/pixel-agent-prompt.md`,
  `pixel-subagent-prompt.md`). Codex emits no retrieval context by default.
  The explicit impact skill and Pi extension treat graph output as repository
  data, not instructions. `impact --no-refresh` uses a read-only graph snapshot,
  verifies extractor and source signatures, and bounds the query to 1500 ms and
  the serialized result to 32 KiB. Its graph completeness claim remains open.
  Other providers' hook packets still carry repository strings as JSON values
  (`[PIXEL:TASK_CONTEXT]` in `prompt_submit.rs`, the dependants list of
  `post-tool-use`) and label them; output is capped.
- **Status**: Accepted: a retrieval tool has to return repository text.
- **Residual**: the defence is the model's; Pixel cannot sanitise meaning.
  Anything that follows from a successful injection is bounded by the
  agent's own permissions, which T1 and T11 can widen.

### T13. Foreign Codex hooks composed into the guard (E, B5)

- **Scenario**: a legacy installation replaced a repository's Codex
  `PreToolUse` hooks with its own `composed-guard`, which replays them.
- **Mitigation**: current installation restores the original registrations
  only when the private backup and managed hook still match their owned
  contract, preserving changed configurations for manual resolution. It skips
  tracked `.codex/hooks.json` (`repo_git::is_tracked`) and project paths that
  alias the global hook file. Duplicate project task hooks are removed only
  when the enabled global suite covers their events and each current definition
  has matching Codex approval; missing or stale approval preserves them. Pixel
  reads approval state but does not grant trust during installation. For explicitly retained
  legacy wrappers, `guard::load_composed_backup`
  refuses a symlink, a file over 1 MiB, a mode wider than 0600, an unknown
  version or provider, and any command that calls Pixel's own hooks
  (`invokes_pixel_hook`). Tests: `composed_*` in `guard.rs` and
  `crates/pixel/tests/cli/guard_deny.rs`.
- **Status**: Partial.
- **Residual**: for a retained legacy wrapper, Codex's review hashes the command line
  of Pixel's composed guard, not the commands in the backup it replays, so
  approving the guard approves what it composes.

### T14. `pixel install` damages or weakens agent configurations (T, I, B5)

- **Scenario**: an install loses a user's settings, or leaves a copy of a
  config that holds keys readable by others.
- **Mitigation**: only managed blocks and hook entries are rewritten; a file
  that does not parse is not touched; each changed file is backed up first;
  `doctor` checks every artifact (`pixel_install::doctor::CHECKS`); hook
  commands quote the executable path.
- **Status**: Partial.
- **Residual**: backups and several configs are written with `fs::write`, so
  they take the umask's mode rather than the original's and follow a
  symlinked config file to its target.

### T15. Secrets at rest on the machine (I)

- **Scenario**: another local user, a backup, or a sync tool reads stored
  keys, flow fill values, transcripts or `.env` copies.
- **Mitigation**: `~/.pixel/config.yaml` is written 0600
  (`config_cmd::write_private`); flows live in a 0700 directory with 0600
  files (`pixel_flow::store::ensure_flow_dir`, `write_atomic`); `.pixel/` is
  0700 and `actions.jsonl` 0600 (`pixel-actionlog`, `open_log_file`); the
  recall directory is 0700 (`pixel_recall::ensure_recall_dir`); the error
  sink is 0700/0600 (`pixel-session`); `pixel-ops` state directories are
  created 0700 (`durable::ensure_dir`).
- **Status**: Partial.
- **Residual**: the recall corpus and the error sink store transcripts and
  argv unredacted; `edit-env` snapshots are plain copies of the `.env`;
  `logged_args` masks only `config remote-key` values and `auth_url`, so a
  secret passed as `--var` to `pixel flow` or `--value` to `pixel edit-env`
  reaches `actions.jsonl` in clear (a 0600 file).

### T16. A configured key is sent to the wrong endpoint (I, B6)

- **Scenario**: a repository's configuration points classify or web search at
  an attacker's server while the key comes from the user's global config.
- **Mitigation**: endpoints, presets and keys are read from the global config
  only (3.7); key variable names are fixed per preset (`Preset::key_env`);
  `decide_remote::sends_in_clear_text` refuses to send a key over `http://`
  to a non-loopback host (`a_key_never_leaves_the_machine_over_cleartext_http`);
  TLS uses webpki roots and `ureq` does not forward `Authorization` across a
  redirect.
- **Status**: Mitigated for the repository layer.
- **Residual**: `PIXEL_REMOTE_BASE` in the environment (direnv, a CI job)
  redirects the endpoint and the key to any HTTPS host.

### T17. Data leaves the machine unexpectedly (I, B6)

- **Scenario**: repository text or prompts reach a third party.
- **Mitigation**: nothing is sent without an explicit command or setting:
  `classify` sends its capped input to the configured endpoint, `web-search`
  sends its query, the prompt hook sends the prompt to the local engine only,
  the release check sends no repository data (SECURITY.md, "Security model").
- **Status**: Mitigated.
- **Residual**: the local engine's base comes from the global config and is
  not restricted to loopback; a non-local base would receive every prompt
  while `classify.enabled` is on.

### T18. A downloaded model or tool is tampered with (T, B6)

- **Scenario**: a compromised Hugging Face repository or install script.
- **Mitigation**: HTTPS only.
- **Status**: Accepted.
- **Residual**: models are fetched at the repository's default revision
  without a pinned revision or checksum; `PIXEL_RECALL_MODEL_REPO` can name
  any repository; the local engine setup runs `ollaya.dev/install.sh` without
  a checksum. Each runs only on an explicit setup or first semantic query.

### T19. A packed index is tampered with (T, B6)

- **Scenario**: `pixel index-unpack https://…` installs a bundle built by
  someone else.
- **Mitigation**: each member's xxh3 is checked against the bundle's own
  manifest; only the files `pixel index pack` writes are accepted as members
  (since 0.7.0); the target `.pixel/` passes `pixel_git::sidecar::check` and
  the staging directory is created without following links; a live index is
  not overwritten without `--force`.
- **Status**: Partial.
- **Residual**: the check proves integrity against corruption, not
  authenticity: a bundle is exactly as trustworthy as whoever produced it and
  where it was fetched from (`http://` is accepted), and it lands in
  `.pixel/` with the trust of T6.

### T20. The update path installs a tampered binary (T, E, B7)

- **Scenario**: the update prompt, `scripts/install.sh` or `pixel
  self-update` installs something other than a genuine release.
- **Mitigation**: `install.sh` and the `setup-pixel` action check the archive
  against the release's `.sha256`; every archive and `install.sh` carry a
  signed build-provenance attestation (SECURITY.md, "Verifying a release");
  the prompt defers to Homebrew or mise when they own the binary.
- **Status**: Partial.
- **Residual**: the `.sha256` comes from the same release, so it proves
  integrity, not provenance; attestation verification is a manual step;
  the update prompt treats an empty answer as yes. `pixel self-update` runs
  `--build` in the current directory and installs whatever binary it
  produced: run it from a Pixel checkout only.

### T21. The release chain is subverted (T, E, B7)

- **Scenario**: a malicious dependency, action or tag produces a signed but
  tampered release.
- **Mitigation**: releases build on GitHub-hosted runners in the reusable
  `release-build.yml`, which holds no secret and is the attestation's signer
  (SLSA Build Level 3); `release.yml` publishes and holds the tap token;
  every remote `uses:` is pinned to a commit (`scripts/verify-action-pins.py`);
  `cargo deny` and `osv-scanner.toml` gate advisories, licences and sources
  (`deny.toml`: crates.io only); Dependabot, OpenSSF Scorecard and CodeQL run
  on `main`; `pixel check-release` gates the tag.
- **Status**: Partial.
- **Residual**: the chain rests on the maintainers' accounts and on the tag
  and branch protections described in GOVERNANCE.md; a user who skips
  attestation verification trusts GitHub's release storage.

### T22. A pull request attacks CI (E, B7)

- **Scenario**: a fork's pull request runs code with repository secrets or
  write tokens (a "pwn request"), or injects through an interpolated title or
  body.
- **Mitigation**: `board-sync.yml` (`pull_request_target`) never checks out
  the pull request, runs with `permissions: {}`, and never reads the
  body: it takes the PR's closing issues from GitHub's GraphQL API; no workflow interpolates `github.event.*` or
  `inputs.*` directly in a `run:` script; workflows default to `contents:
  read`; runs of outside contributors wait for a maintainer's approval;
  CodeQL always scans workflows, Python and JavaScript with default security
  queries on every PR. Rust analysis runs after merges to main, nightly and
  on manual dispatch; a sensitive branch can be selected for a pre-merge
  scan. The CodeQL merge-protection rule retains its error/medium-security
  alert thresholds. The main ruleset also requires all three PR analysis
  jobs from GitHub Actions; a missing or failed analysis cannot be hidden by
  the aggregate CodeQL check's neutral warning about the omitted Rust config.
  Its Rust extraction cache is separate from build/test caches;
  only successful main analyses save executable build-script/proc-macro
  outputs, while manual branch analyses may restore them. A cache hit never
  replaces an analysis.
- **Status**: Partial.
- **Residual**: Rust CodeQL findings may be discovered after merge; triage
  them before the next release. Any pull request can link an issue with a
  closing keyword (`Closes #<n>`) and move that issue's board status; workflows that compile pull-request code on the
  persistent self-hosted runner depend on that runner's isolation, which is
  an operational control outside this repository.

### T23. Repository configuration is code (E, B1)

- **Scenario**: a repository's `.pixel/config.yaml` declares `task.checks`,
  and the agent runs `pixel task-state verify` in it.
- **Mitigation**: checks run as argv without an implicit shell, in a private
  materialized copy of the source (`pixel_task::snapshot::materialize`), with
  a working directory confined to that copy and a reduced environment.
- **Status**: Accepted: a repository's checks are as trusted as its
  `Makefile` or test suite.
- **Residual**: the copy is not a sandbox: checks run with the user's
  privileges and network. Do not verify a task in a repository you would not
  build.

### T24. Actions cannot be attributed (R)

- **Scenario**: after an incident, nobody can tell which agent ran which
  Pixel mutation.
- **Mitigation**: `actions.jsonl` records each invocation; `pixel-ops`
  journals each guarded mutation under `~/.local/state/pixel/journals/`.
- **Status**: Accepted.
- **Residual**: both are local, best-effort and writable by the same user;
  they help debugging, not forensics.

### Summary

| Status | Threats |
| --- | --- |
| Mitigated | T4, T10, T16, T17 |
| Partial | T2, T3, T5, T6, T8, T11, T13, T14, T15, T19, T20, T21, T22 |
| Accepted | T1, T7, T9, T12, T18, T23, T24 |

## 5. Critical paths and how they are tested

| Path | Code | Tests |
| --- | --- | --- |
| Daemon request framing | `daemon::handle_conn`, `read_capped_line` | `oversized_line_is_rejected_without_unbounded_drain`, `expired_connection_deadline_stops_frame_read`, `socket_identity_should_follow_the_file_not_the_path`, `the_daemon_socket_should_be_0600_after_bind` (`daemon.rs`) |
| Protocol skew | `classify_ping`, `PROTOCOL_VERSION` | `op_name_matches_serde_tag`, `session_capabilities_track_every_real_op` (`pixel-proto`) |
| Planted history database | `FactsStore::needs_rebuild`, `_pixel_marker` | `open_should_wipe_a_planted_history_database` (both refusals: no marker, foreign `created_by`; asserts the planted tables are gone and the marker is Pixel's); `concurrent_open_on_poisoned_db_never_ioerrors` covers the rebuild path |
| Git argument handling | `validate_ref`, `end_of_options`, `GitRunner` | `rejects_leading_dash` and siblings in `ref_guard.rs`; `only_pixel_git_spawns_git_in_production_code` and `pixel_git_spawns_git_only_in_the_runner` (`crates/pixel-git/tests/boundary.rs`) |
| Guard rewrite and permission | `search_compat::shell_argv`, `shell_quote`, `retrieval_permission_response` | `shell_parser_is_conservative_and_keeps_quoted_words`, the `permission_*` tests in `guard.rs`, `crates/pixel/tests/cli/guard_deny.rs` and `guard_enforce.rs` |
| Composed foreign hooks | `load_composed_backup`, `run_foreign_command` | `composed_backup_replays_foreign_hooks_and_refuses_pixel_under_either_verb`, `composed_codex_*` in `guard_deny.rs` |
| Secrets in the action log | `logged_args` | `only_the_remote_key_command_starts_the_mask` (`main.rs`) |
| Keys over clear text | `sends_in_clear_text` | `a_key_never_leaves_the_machine_over_cleartext_http`, `only_plain_http_to_another_host_counts_as_clear_text` |
| Untrusted shard files | `Shard::open` | `malformed_shard_rejected_gracefully`, `corrupt_posting_cannot_escape_section_or_overflow_delta` (`shard.rs`) |
| Source parsing | `pixel_graph::extract::extract_file` | `graph_extract` fuzz target |
| Query planning | `pixel_index::plan::plan_pattern` | `search_plan` fuzz target |
| Install edits | `pixel-install` | `crates/pixel-install/tests/install_tests.rs`, `install_exit.rs` and `uninstall_cli.rs` (CLI) |
| Release consistency | `pixel-release` | `pixel check-release` in `release.yml`'s `verify` job |

Across all of them:

- **Mutation testing**: `mutants.yml` checks main's cumulative diff nightly,
  only when it has unjudged commits. Survivors and incomplete campaigns
  fail that run and require follow-up; this is post-merge detection, not a
  condition of merge. A test-only weakening can escape a diff campaign;
  that limitation is accepted by the nightly cumulative-diff policy.
- **Fuzzing**: `fuzz.yml` runs every cargo-fuzz target for 60 s on a pull
  request touching `fuzz/`, `pixel-graph`, `pixel-index`, `pixel-git`, the
  root `Cargo.toml` or `deny.toml`, for 600 s weekly, and for 120 s on every
  `v*` tag before `release.yml` builds anything.
- **Static analysis**: `codeql.yml` scans workflows, Python and
  JavaScript/TypeScript on every pull request into `main`. Rust runs after
  merge, nightly at 05:41 UTC and manually on a selected branch; those
  events scan all four languages. Rust findings require triage before the
  next release; `cargo clippy` with warnings denied remains in PR CI.
- **Dependencies**: `cargo deny check` and `scripts/check-advisory-ignores.py`
  in CI; Dependabot weekly for Cargo and GitHub Actions.

Paths with no dedicated test today are named in the table; they are the
first candidates for one.

## 6. Keeping this document true

- **When it is reviewed.** A pull request that changes one of the trust
  boundaries B1 to B7 updates the matching section here in the same pull
  request: a new entry point, a new file under `.pixel/` or the machine-wide
  state, a new network destination, a new secret, a new hook or install
  target, a new op on the socket, a change to a listed mitigation, or a
  change to a workflow's triggers, permissions or secrets. CONTRIBUTING.md's
  "Definition of done" carries this line, beside its `ARCHITECTURE.md` line.
- **At each release.** Step 3 of the `release` skill reads the new
  `## [x.y.z]` changelog section against section 4; a change that crossed a
  boundary without updating this file gets a `docs` pull request before the
  tag (the prepare pull request itself may only carry version lines and the
  changelog).
- **When a vulnerability is fixed.** The advisory's fix updates the matching
  threat (status and residual) once the advisory is published, not before.
- **Name the code.** Like ARCHITECTURE.md
  ([`.agents/rules/architecture-doc.md`](../.agents/rules/architecture-doc.md)),
  a mitigation is quoted as the code spells it and checked against the tree
  before the push; a mitigation that no longer exists is deleted, not kept
  "for history".
