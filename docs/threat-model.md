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
- **Withdrawn**: the mechanism the scenario needs no longer exists.

## 1. Scope and assets

Pixel is a local CLI (`crates/pixel`, binary `pixel`), a per-repository
daemon (`pixel-daemon`) reached over a Unix socket, hooks it installs into
coding agents (`pixel-install`), and the release chain that ships the binary.
Out of scope: the agent hosts themselves (Claude Code, Codex, Pi, …), the
model providers, and the website under `website/`.

| Asset | Where | Why it matters |
| --- | --- | --- |
| Source and history of indexed repositories | the working tree, read through `pixel-git::GitRunner` and the walkers in `pixel-index`/`pixel-graph` | confidentiality of code the user did not mean to share; integrity of what the agent is told about it |
| Per-repository index and sidecars | `.pixel/` (ARCHITECTURE.md, "On-disk state"): `base.shard`, `delta.shard`, `graph.v2.db` (`pixel_daemon::api::GRAPH_DB_FILE`), `history.db`, `code-vectors/`, `targets.json`, `regions.json`, `actions.jsonl`, `brief-decisions.jsonl`, `config.yaml`, `tasks/`, `env-snapshots/` | what the agent reads as ground truth; `actions.jsonl`, `brief-decisions.jsonl` (the typed prompt) and `env-snapshots/` can hold secrets; `regions.json` is evidence for harness orchestration (agent count, scheduling, merging), never an action recommendation |
| Machine-wide state | `~/.pixel/config.yaml` (remote keys), `~/.local/share/pixel/flows/` (fill values: passwords, OTPs), `~/.local/share/pixel/recall/` (agent transcripts), `~/.local/share/pixel/models/`, `~/.local/state/pixel/` (`pixel-ops` journals, snapshots, locks; the `pixel-session` error sink) | secrets at rest, and transcripts that quote them |
| Daemon socket | `pixel_daemon::daemon::socket_path`: `$TMPDIR` on macOS, `$XDG_RUNTIME_DIR` or `~/.cache/pixel/sockets/` on Linux | any client of the socket can ask for git mutations on the repository |
| Agent configurations | what `pixel install` writes: `~/.claude/settings.json`, `$CODEX_HOME/config.toml` and `hooks.json`, the Pi package under `~/.local/share/pixel/pi-package/` and its entry in Pi's `settings.json`, the optional classify helpers once accepted (`skills/pixel-classify/` under the agent config dirs, the Pi package `~/.local/share/pixel/pi-classify/`); per repository with `--repo`, `.claude/settings.local.json` and `.codex/`. It also edits `~/.pi/agent/APPEND_SYSTEM.md`, the OpenCode, Antigravity, zcode, Devin, Cursor and Copilot CLI configs, `.devin/config.local.json`, `.pi/extensions/pixel-guard.ts` and `AGENTS.md`, only to remove what earlier releases wrote | a hook command runs with the user's privileges on every agent tool call |
| User secrets | provider keys (`OPENROUTER_API_KEY`, `OLLAMA_API_KEY`, `DEEPSEEK_API_KEY`, `OPENCODE_API_KEY`, `TYPESAFE_API_KEY`, `CLOUDFLARE_API_TOKEN` (or `CLOUDFLARE_AUTH_TOKEN`), `PERPLEXITY_API_KEY`, `remote_keys` in the global config, or secrets read from a configured Infisical project), `.env` values edited by `pixel edit-env` | credential theft, billing abuse |
| Benchmark credentials | OAuth storage or an explicitly selected Claude gateway settings file read by `eval/claude_skill_pair.py` | credentials must reach only the selected model connection and stay out of benchmark receipts |
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
| Network services | untrusted for integrity, trusted with what is sent to them | Hugging Face, the classify endpoints (including `api.typesafe.ai`, `api.cloudflare.com` for Clef-flash on Workers AI, and a user-configured Ollama host for Clef-flash through Ollama), a configured Infisical host (default `app.infisical.com`, sent the bearer token), the web-search endpoints, `github.com` release checks, `ollaya.dev` |
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
`PROTOCOL_VERSION` (`pixel_daemon::api`, 16) is compared by the CLI's
`classify_ping`: an older daemon is shut down and replaced, a newer one is
left alone and the command runs in process. Requests are served one at a
time. The op set includes git mutations (`publish`, `push`, `ship`,
`branch_op`, `update`, `sync`, `reconcile`), file writes (`rename`), index
rebuilds and `shutdown`. The CLI starts a daemon on demand
(`auto_start_daemon`; `PIXEL_DAEMON_AUTO_START=0` turns that off).

`meaning` takes a free-text question (cut to `MEANING_QUERY_MAX_BYTES`, 1 KiB)
and returns at most `MEANING_MAX_LIMIT` (50) one-line snippets from the
resident code index (ARCHITECTURE.md, "Daemon and wire contract"). A request
reads no file, embeds one question and starts no download; the index is built
on one background thread from the files `search-meaning` would read, under its
caps (`RESIDENT_MAX_FILES`, 512 KiB per file), and never from a path
`credential_path` names. Repeated edits do not grow it for the daemon's
lifetime: a rebuild renumbers the token vocabulary from the live chunks once
the tokens of edited-away text outnumber them (`VOCAB_SLACK`). A same-user client can make the daemon read and
embed the repository by sending it, which it could do with `search`.

### 3.3 Installed hooks (B2, B3, B5)

`pixel run-hook task-event` reads one JSON payload from the agent host on
stdin (ARCHITECTURE.md, hook table). The payload's `cwd`, `tool_input` and
session fields are agent-controlled. `task-event` is the only hook `pixel
install` registers: it gates edits and completion for a task contract and, on
`prompt-submit`, returns the bounded `[PIXEL:BRIEF]` evidence block. It never
executes `tool_input.command` and never rewrites or approves a command.

The verbs earlier releases registered (`guard`, `composed-guard`,
`session-start`, `prompt-submit`, `post-compaction`, `post-tool-use`,
`metrics`) still parse, exit 0 and print nothing, so a registration an old
install left behind can neither block a host nor add text to its context.

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

The reference manifest (`.pixel/reference.json`, `reference_cmd`) is read back
the same way, but its entries carry revisions that become git argv in
`reference setup`: every revision passes through `pixel_git::validate_ref`
both when an entry is added and when the manifest is loaded, so a
hand-edited manifest cannot smuggle a flag-shaped revision into `git fetch`
(see T8).
The verified-history store (`.pixel/classify-history.jsonl`,
`classify_history::HistoryStore`) follows the same rules: both openers run
`sidecar::check` first, so a tracked or linked `.pixel/` is refused rather
than read; the write path creates `.pixel/` with `sidecar::private_dir`; and
`save` replaces the file through `nofollow::write_replace`, so a pre-planted
`.tmp` symlink cannot redirect the write.

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

`pixel reference setup` clones or fetches each corpus named in
`.pixel/reference.json` under `.pixel/references/`: the repository URL is
passed after `--` to `git clone`, the pinned revision is validated by
`validate_ref` before it reaches `git fetch`/`git checkout`, and the fetched
commit is checked out detached — so a pin is never left on a stale local
branch and a fetched revision is used as the object, not re-resolved from a
local ref.

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
  The `openai` preset posts to `https://api.openai.com/v1/decisions` with
  `OPENAI_API_KEY` (or a key stored by `pixel config remote-key openai`).
- `pixel classify` on the `clef-cloudflare` preset (`crates/pixel/src/decide_clef.rs`):
  `POST https://api.cloudflare.com/client/v4/accounts/<account>/ai/run/@cf/cloudflare/<model>`
  (Workers AI) with the API token as a Bearer `Authorization` header
  (`CLOUDFLARE_API_TOKEN`, else `CLOUDFLARE_AUTH_TOKEN`, else `pixel config
  remote-key clef-cloudflare`). The account id comes from `CLOUDFLARE_ACCOUNT_ID`
  or a stored base ending `/accounts/<id>`; it becomes a URL path segment, so
  `cloudflare_account_base` accepts only an alphanumeric one. The `clef-ollama`
  preset posts the same typed-questions request to `<base>/v1/systemone` of an
  Ollama server (default `http://127.0.0.1:11434`); a local host needs no key, a
  remote one takes `OLLAMA_API_KEY` as a Bearer token. The state, context and
  criteria are sent, capped like the other presets.
- The prompt-submit brief's optional intent judge carries the full prompt
  only to `decide_ollaya::DEFAULT_BASE` (loopback), passed explicitly with
  `--if-warm --engine ollaya --ollaya-url`. It does not inherit the configured
  classify engine or endpoint, does not start the local engine, and never
  falls back to a remote provider; a cold engine leaves the heuristic plan
  unchanged (`execution_brief::intent::judge`, 400 ms child budget).
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
- `pixel reference setup`: `git clone`/`git fetch` (through `GitRunner`) from
  each repository URL recorded in `.pixel/reference.json`; the manifest is
  user-authored, so no destination is invented and no URL is guessed.
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

### 3.11 Isolated Claude benchmark (B2, B6)

`eval/claude_skill_pair.py` accepts either existing OAuth credentials or an
explicitly selected gateway settings file. Gateway mode forwards only its
allowlisted connection and model environment fields to both isolated arms;
it does not load the file as agent settings or forward an API key. OAuth
credentials use private temporary files. Receipts omit credential values and
hashes. Before Claude output is parsed or saved, exact OAuth access and refresh
token values (including their JSON-escaped forms) and known private gateway
connection values are redacted from stdout and stderr. This is exact-value
redaction, not general secret detection: transformed or otherwise unknown
secret forms may remain. It prevents accidental recording of the known values,
not access by the user's other processes or deliberate reads by an agent running
as that user; the temporary config and output files are not an OS security
sandbox.
The selected endpoint receives the benchmark's source context. CLI-reported
model names do not attest the gateway's underlying implementation.

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
  through a shell. The reference manifest's revisions are validated on load
  and on add (`reference_cmd`), so a hand-edited `.pixel/reference.json` is
  refused rather than forwarded to `git fetch`.
- **Status**: Partial.
- **Residual**: pathspec magic (`:/`, `:(glob)`) is not disabled; a remote
  may be a local path (`/abs/repo`), which `validate_ref` accepts; refs read
  from the repository's own state are not all passed through `validate_ref`
  (the reference manifest is one that is).

### T9. Repository hooks run on `pixel commit` and `pixel push` (E, B1)

- **Scenario**: `pre-commit`, `commit-msg` or `pre-push` hooks of the
  repository run when the agent publishes.
- **Mitigation**: none; Pixel never passes `--no-verify`, by design.
- **Status**: Accepted: the same hooks run on the user's own `git commit`.
- **Residual**: `GitRunner` kills git on timeout, not its process group, so a
  hook's children can outlive the 120 s timeout.

### T10. A crafted hook payload abuses a hook (T, E, B2)

- **Scenario**: the agent (or text injected into it) shapes the `cwd`,
  `session_id` or `tool_input` of a `task-event` payload so that the hook
  executes something else, binds another session's task, or reads outside the
  repository.
- **Mitigation**: the hook never executes `tool_input.command`; it reads its
  payload through one bounded reader, `hook_input::read_bounded`, capped at
  `MAX_HOOK_INPUT` (1 MiB), so an over-cap payload is refused after `cap + 1`
  bytes instead of being allocated in full; over-cap `task-event` input emits
  its unavailable response, which denies `PreToolUse` on an enforced session;
  the task binds by worktree, provider and session (ARCHITECTURE.md, Agent
  integration); `pixel install` registers a 10 s timeout (`HOOK_TIMEOUT`).
- **Status**: Mitigated.
- **Residual**: the cap bounds allocation, not the wait: a host that writes
  fewer than `MAX_HOOK_INPUT + 1` bytes and holds the pipe open still stalls
  the hook until the host's `HOOK_TIMEOUT` (10 s) ends it.

### T11. Withdrawn

The hook that approved Pixel retrievals without a prompt no longer exists, and
its verb answers nothing, so a stale registration approves nothing. The id
stays so references to the later threats hold.

### T12. Prompt injection through Pixel's output (T, B3)

- **Scenario**: a repository plants instructions in code, comments, commit
  messages, symbol names or file names; Pixel quotes them to the agent, which
  follows them.
- **Mitigation**: the one thing an install delivers to an agent unasked is
  the `[PIXEL:BRIEF]` block of `task-event` on `prompt-submit`: it quotes file
  paths, symbol names and a definition from the repository as evidence, capped
  at 2 KiB (`BRIEF_BYTES`), with a 750 ms deadline, off with `PIXEL_BRIEF=0`.
  A prompt in plain language gets one only when the repository's own
  vocabulary covers it (`execution_brief::relevance`), and its first files
  come from the daemon's `facts.relevance` and `meaning` ops: the same
  deadline, the same data. A confident brief of such a prompt (or of a weak
  one) also quotes multi-line source excerpts, comments included: per chunk
  the signature, its first doc line and up to six body lines, or a flow's
  hops, a test's first assertion, a config constant, under a 3584-byte cap
  (`PROSE_BRIEF_BYTES`, below Pi's 4000-byte limit). `Evidence::lines_at`
  reads them only from files the searches named, refuses a credential-shaped
  path, and `PIXEL_BRIEF_ANSWER=0` drops them; the "Answer from this
  evidence" directive stays off unless `PIXEL_BRIEF_DIRECTIVE=1`, so by
  default the block asks the agent to verify them. No prompt is deployed. The bundled prompt a user copies by hand
  states that Pixel output is data, not instructions
  (`crates/pixel-install/assets/pixel-agent-prompt.md`). The explicit impact
  skill and Pi command label graph output as repository data, not
  instructions. `impact --no-refresh` uses a read-only graph snapshot,
  verifies extractor and source signatures (re-hashing only files whose mtime
  is not older than the last full build), and bounds the query to 1500 ms and
  the serialized result to 32 KiB. Its graph completeness claim remains open.
- **Status**: Accepted: a retrieval tool has to return repository text.
- **Residual**: the defence is the model's; Pixel cannot sanitise meaning.
  Anything that follows from a successful injection is bounded by the
  agent's own permissions, which T1 can widen.

### T13. Withdrawn

Pixel no longer composes a repository's Codex `PreToolUse` hooks behind its own
wrapper. `pixel install --repo` restores the original registrations from the
private backup of an earlier release only while the backup and the managed
hook still match their owned contract, skips tracked `.codex/hooks.json` and
paths that alias the global hook file, and removes duplicate project task
hooks only when the enabled global suite covers their events and Codex has
approved each current definition (T14). A leftover `composed-guard`
registration answers nothing. The id stays so references to the later threats
hold.

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
  created 0700 (`durable::ensure_dir`); `brief-decisions.jsonl`, which keeps
  the typed text of each prompt the brief judged, is created 0600, takes no
  line when an existing file cannot be brought to 0600, and is
  written without following a link at the file or at `.pixel` itself
  (`decision_log::append`, `nofollow::open_lock`), holds the last 500
  lines, never a pasted block (`typed_text`) or more than the last
  paragraph of a long untagged paste (`brief_task`), masks credential shapes
  before it cuts the text at 600 characters (`decision_log::logged_typed`:
  `pixel_git::redact` and a key mask), and is off with
  `PIXEL_BRIEF_LOG=0|false|off`.
- **Status**: Partial.
- **Residual**: the recall corpus and the error sink store transcripts and
  argv unredacted (the sink keeps the whole output of every failed `sniper
  run`, structured RSpec/RuboCop/Minitest runs included, for 7 days and
  at most 200 outputs); `edit-env` snapshots are plain copies of the `.env`;
  `logged_args` masks only `config remote-key` values and `auth_url`, so a
  secret passed as `--var` to `pixel flow` or `--value` to `pixel edit-env`
  reaches `actions.jsonl` in clear (a 0600 file); the brief log masks key
  shapes and URL credentials, not a secret typed as ordinary words, which
  stays in that 0600 file until the log wraps or is deleted.

### T16. A configured key is sent to the wrong endpoint (I, B6)

- **Scenario**: a repository's configuration points classify or web search at
  an attacker's server while the key comes from the user's global config.
- **Mitigation**: endpoints, presets and keys are read from the global config
  only (3.7); key variable names are fixed per preset (`Preset::key_env`);
  the OpenAI preset uses `OPENAI_API_KEY` for
  `https://api.openai.com/v1/decisions`;
  `decide_remote::sends_in_clear_text` refuses to send a key over `http://`
  to a non-loopback host (`a_key_never_leaves_the_machine_over_cleartext_http`);
  TLS uses webpki roots and `ureq` does not forward `Authorization` across a
  redirect.
  The Clef-flash presets (`decide_clef.rs`) apply the same cleartext refusal to a
  stored Ollama or Cloudflare base; their key travels only in the
  `Authorization` header, never in a URL, a log line, an error or a document.
- **Status**: Mitigated for the repository layer.
- **Residual**: `PIXEL_REMOTE_BASE` in the environment (direnv, a CI job)
  redirects the endpoint and the key to any HTTPS host.

### T17. Data leaves the machine unexpectedly (I, B6)

- **Scenario**: repository text or prompts reach a third party.
- **Mitigation**: nothing is sent without an explicit command or setting:
  `classify` sends its capped input to the configured endpoint (including
  `https://api.openai.com/v1/decisions` for the OpenAI preset), `web-search`
  sends its query, and the automatic prompt-submit brief judge is pinned to an
  already-warm loopback Ollaya endpoint with no remote fallback; the release
  check sends no repository data (SECURITY.md, "Security model").
- **Status**: Mitigated.
- **Residual**: an explicit `pixel classify` invocation may use a configured
  remote provider or non-loopback local endpoint; the prompt hook never makes
  that choice on the user's behalf.

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
| Partial | T2, T3, T5, T6, T8, T14, T15, T19, T20, T21, T22 |
| Accepted | T1, T7, T9, T12, T18, T23, T24 |
| Withdrawn | T11, T13 |

## 5. Critical paths and how they are tested

| Path | Code | Tests |
| --- | --- | --- |
| Daemon request framing | `daemon::handle_conn`, `read_capped_line` | `oversized_line_is_rejected_without_unbounded_drain`, `expired_connection_deadline_stops_frame_read`, `socket_identity_should_follow_the_file_not_the_path`, `the_daemon_socket_should_be_0600_after_bind` (`daemon.rs`) |
| Protocol skew | `classify_ping`, `PROTOCOL_VERSION` | `op_name_matches_serde_tag`, `session_capabilities_track_every_real_op` (`pixel-proto`) |
| Snippets from the semantic index | `Resident::build`, `Meaning::answer` | `build_should_never_index_credential_shaped_paths` (`code_resident.rs`), `answer_should_apply_the_default_limit_and_cap_the_requested_one` and `bounded_query_should_cut_at_a_character_boundary_above_the_cap_only` (`meaning.rs`) |
| Planted history database | `FactsStore::needs_rebuild`, `_pixel_marker` | `open_should_wipe_a_planted_history_database` (both refusals: no marker, foreign `created_by`; asserts the planted tables are gone and the marker is Pixel's); `concurrent_open_on_poisoned_db_never_ioerrors` covers the rebuild path |
| Git argument handling | `validate_ref`, `end_of_options`, `GitRunner` | `rejects_leading_dash` and siblings in `ref_guard.rs`; `only_pixel_git_spawns_git_in_production_code` and `pixel_git_spawns_git_only_in_the_runner` (`crates/pixel-git/tests/boundary.rs`) |
| Hook payload cap | `hook_input::read_bounded` | the `read_bounded` tests in `hook_input.rs` |
| Prompt-submit intent judge | `execution_brief::intent::judge` | subprocess JSON, empty stdout and invalid UTF-8 fallback tests in `intent.rs` |
| Typed prompts in the brief log | `decision_log::{logged_typed, mask_keys, append}` | `logged_typed_should_mask_a_credential_and_cut_at_the_bound`, `logged_typed_should_keep_only_the_last_paragraph_of_a_pasted_log`, `mask_keys_should_hide_a_prefixed_key_and_a_long_run_and_keep_prose`, `append_should_be_owner_only_and_not_create_a_missing_directory`, `append_should_refuse_a_link_at_the_log_and_leave_its_target_alone`, `append_should_refuse_a_linked_directory_and_write_nothing_through_it` (`decision_log.rs`), `the_decision_log_should_stay_off_when_pixel_brief_log_says_so` (`prompt_brief_cli.rs`) |
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
