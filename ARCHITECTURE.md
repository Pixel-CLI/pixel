# Architecture

The map a contributor reads before touching two crates. The product thesis and
every contract live in [`specs/`](specs/); this file says where they live in
the code and what the tests hold in place. `cargo test -p pixel --test cli
docs_drift::` fails when the two tables below disagree with the tree.

Pixel avoids repeated investigation: it computes, with no model, the evidence an agent
would otherwise fetch with tool calls, and pushes it through hooks
(`specs/thesis.md`). This file describes the candidate implementation, including
the hook handlers, setup/doctor commands and the `pixel-setup` crate.

## Command surface

One row per subcommand of `pixel --help`, in that order.

| Command | What it does |
| --- | --- |
| `prepare` | Builds or refreshes the text index and the code graph (and, with `--history`, the history db). |
| `status` | Reports the freshness of the index, the graph and the history db. |
| `search-content` | Regex search over the trigram index, ripgrep-style (`-g`, `-t`, `-l`, paging). |
| `search-meaning` | A question in your own words ranked over code chunks (symbols with their doc comments, windows for other files): semantic rank fused with BM25. Leads, never verdicts. Needs the `semantic` build and its cached model; `--download` fetches it, the one place that does. |
| `find-code` | Finds where a phrase lives: the string literals, UI text, routes, environment-variable names and config keys that carry its words, ranked by kind, each with the symbol that owns the line. There is no stored concept table: it reads the text index (`pixel_graph::analysis::concept`). |
| `find-symbol` | Finds a symbol by name or uid. |
| `who-calls` | The callers of a symbol, or with `--callees` what it calls, each with its tier. |
| `impact` | The blast radius of a symbol by depth, with the tests that cover it. |
| `what-changed` | Maps the working tree (or a base) onto the symbols it changes. |
| `review` | Deterministic review: readers a change left behind, untested changes, credential-shaped additions. |
| `call-path` | The shortest chain of resolved calls between two symbols. |
| `list-signatures` | The declarations of one file. |
| `pack-context` | One symbol's source fitted to a token budget (`--budget`, four bytes a token): its doc comment, its declaration and as much of the body as fits, with the lines left out counted. |
| `repo-map` | Every indexed file with its symbols. |
| `search-history` | Searches commit messages, paths and diffs. |
| `rename` | Renames a symbol through the sites the graph resolved to it; a dry run unless `--apply`. |
| `brief` | The brief a prompt would get, computed by the daemon with no model; the decision goes to the journal. `--without STEP` leaves recipe steps out (`pixel_brief::steps`). |
| `hook` | Answers one agent-host hook event from the payload on stdin: one JSON envelope or nothing, always exit 0. |
| `setup` | Registers hooks, plugin, package and skill with the agents on the machine, or removes them (`--remove`); `--print` writes nothing. At a terminal with no `--agent`, `--print`, `--json` or `--yes` it asks one question per agent (`pixel_setup::setup::wizard`). |
| `doctor` | Green/yellow/red checks of the install, the binary, the daemon and the freshness of the indexes, each flagged one with its repair command; `--fix` runs each distinct repair once. |
| `config` | Lists the settings in force with where each value comes from (environment, repository, global, default), or sets or unsets one (`metrics`, `daemon_auto_start`, `brief`, `brief_parts`, `edit_hooks`). |
| `metrics` | The report of `.pixel/journal.jsonl` and `.pixel/actions.jsonl` over a window (`--since`): briefs delivered and declined by tier and kind, the zero-call turn rate, precision and recall of injected versus read files, edit cards, stale sites and Stop blocks, and the commands' estimated savings by estimator version; measured and estimated numbers are labelled apart. |
| `classify` | The opt-in zero-shot decision over a bounded label set: probabilities per label from a remote OpenAI-compatible chat, OpenAI Decisions, Jev, Clef-flash or a local Ollaya engine; off until `pixel config classify on`, no call in any default path. `--task-intent` judges a prompt against the built-in task labels. |
| `workspace` | `add`, `remove`, `list` and `clear` the repositories that `impact --workspace` and `who-calls --workspace` fan out across; each member answers from its own index and the results stay per repository. |
| `index` | `pack` freezes `.pixel/`'s index files (`base.shard`, `state.json`, `graph.db`, and with `--include-history` `history.db`) into one `.pixelpack` with an xxh3 manifest; `unpack` verifies every file against it and installs it (path or `https://` URL), refusing while a daemon answers unless `--force`. |
| `install` | Adds a plugin from a directory, a git URL or `github:<owner>/<repo>[@<ref>]` (a release asset is checked against its `.sha256`), and delivers the skill its manifest declares. |
| `uninstall` | Removes an installed plugin and the skill copies it delivered that are still unchanged. |
| `plugin` | `list`, `trust`, `enable` and `disable` plugins; a repo plugin runs only at the digest `trust` recorded. |
| `daemon` | Starts, stops or checks the repository's daemon. |

`help` is clap's own. Every command prints a `🟩 pixel <cmd> ❀ <ms> ❀ #<id>`
block on stderr after its answer (`--metrics off` silences it, never the
accounting). `--json` prints the op's result instead of text; `--no-daemon`
answers in the CLI's own process.

## Crates

One row per workspace member; `Depends on` is the `pixel-*` entries of its
`[dependencies]`.

| Crate | Role | Depends on |
| --- | --- | --- |
| `pixel-io` | The one bounded `git` runner and the sidecar trust boundary: `GitRunner`, `nofollow`, `sidecar`, `repo_path`. Nothing else in the workspace spawns `git` (`tests/boundary.rs`). | — |
| `pixel-model` | Pure types and rules: graph rows (`Symbol`, `Edge`, `Tier`, `Lang`), the `Graph` trait, `Diff`, `is_test_path`, the lexicon. | — |
| `pixel-proto` | The wire contract: the 15 `Op`s, the `Envelope`, error codes, snapshot, epistemics. No I/O. | — |
| `pixel-metrics` | The action log (`.pixel/actions.jsonl`), the workflow estimates and the 🟩 line, the CLI's output counters, and the hook journal (`.pixel/journal.jsonl`: the `brief` decision and the `prompt`/`part`/`tool`/`stop`/`turn` lines). | `pixel-io`, `pixel-model` |
| `pixel-plugin` | The plugin host (`specs/plugins.md`): manifests (`pixel-plugin.toml`), the lookup order, the trust store keyed by a digest of the plugin directory, install from a directory, a git URL or a checked release asset through directory descriptors that refuse links, and the argv and environment a plugin runs with. | `pixel-io` |
| `pixel-graph` | The code graph: one pipeline (`tree` → `extract` → `link`), a store, rules per language (`rust`, `ruby`, `names`), shapes, the edit check, the analyses, rename. | `pixel-model`, `pixel-io` |
| `pixel-index` | The git-anchored trigram text index, the history store and the semantic chunker, vector store and ranking. | `pixel-io`, `pixel-model` |
| `pixel-brief` | The prompt brief: typed-text signal, the relevance gate (`gate_model`), the question kind (`routes`), the evidence chain over an `Evidence` trait (`chain`), the recipe steps with their ablation flags (`steps`), the third-party brief-probe runner (`probes`), the region pack, render and parts. Reads no store; the daemon implements `Evidence`. | `pixel-io`, `pixel-metrics`, `pixel-model`, `pixel-plugin` |
| `pixel-daemon` | The Unix-socket NDJSON daemon: serve loop, `Service::handle`, one `Reader` for the writer and its replicas, `brief/` (the `Evidence` of a brief over the stores it holds, the relevance counts the gate scores) and `edit/` (the card before an edit, the stale callers after it, the Stop block, and the session memory they share). | `pixel-brief`, `pixel-graph`, `pixel-index`, `pixel-io`, `pixel-metrics`, `pixel-model`, `pixel-plugin`, `pixel-proto` |
| `pixel-setup` | The host table (the hosts PIXEL serves, the event each fires and what it is called there, the payload fields read from each, the envelope each accepts, the metrics relay) and the installers that register it: hook entries, OpenCode's plugin, pi's package, Antigravity's plugin, the `pixel` skill, the ownership receipt, uninstall, the migration off a Pixel install, the doctor's file-based checks, the typed settings of `config.yaml`, and the delivery of a plugin's skill. | `pixel-io`, `pixel-model`, `pixel-plugin` |
| `pixel` | The binary: the clap surface, the daemon client, rendering, the invocation lifecycle, and `hook/` (the one entry point every host's registered hook runs). The opt-in classifier lives here too (`classify`, `classify_setup`, `decide_*`, `classify_config`, `task_intent`) behind `pixel classify`. | `pixel-brief`, `pixel-daemon`, `pixel-graph`, `pixel-index`, `pixel-io`, `pixel-metrics`, `pixel-model`, `pixel-plugin`, `pixel-proto`, `pixel-setup` |

## On-disk state

Under `<repo>/.pixel/` (owner-only; a `.pixel` that is a link or that git tracks is
refused, `pixel_io::sidecar::check`):

- `graph.db` (`pixel_graph::store`, schema `SCHEMA_VERSION`): the published
  generation, one writer, rebuilt when the schema, the extractor version or the
  stamp says it cannot be trusted. Written by `pixel-graph` only.
- `base.shard` (`SHARD_FILE`), `delta.shard` (`DELTA_FILE`), `state.json`
  (`STATE_FILE`) and `build.lock`: the text index. Written by `pixel-index` only.
  The shared cache of base shards is `$XDG_CACHE_HOME/pixel/shards`.
- `history.db` (`pixel_index::history`, `FACTS_SCHEMA_VERSION`): created by the
  first history query, never by `status` or a read-only query.
- `code-vectors/` (`pixel_index::semantic::store::STORE_DIR`): the embedded code
  chunks of the `semantic` feature: `manifest.json`, immutable `seg-*.vec`
  segments of `i8` rows keyed by the xxh3-128 of model, revision,
  `CHUNKER_VERSION` and chunk text, and a `lock`. Written by the daemon's
  warmer only, and only for a root that already has a `.pixel/` and is not
  `$HOME`; rewritten when a quarter of its rows are unreachable.
- `actions.jsonl` (`pixel_metrics::LOG_FILE_NAME`): one line per CLI invocation,
  rotated at 5 MiB. Written by `pixel-metrics` only.
- `journal.jsonl` (`pixel_metrics::journal::FILE_NAME`): one typed line per hook
  decision, the last `MAX_LINES` kept, `PIXEL_LOG=0` writes nothing. Written by
  `pixel-metrics` only, from the line each caller builds.
- `brief-sessions/` (`pixel_brief::sent::DIR`): the ranges of region packs a
  session was already sent, one file per hashed session, pruned after `TTL`.
  Written by `pixel-brief` only.
- `brief-parts/` (`hook::parts::CACHE_DIR`): the brief of a prompt that several
  hooks deliver in parts, stored whole by part 1 with a `.done` marker; pruned
  after `MAX_AGE`. Written by `pixel hook` only.
- `sessions/<key>.json` (`pixel_daemon::edit::session::DIR`): what the edit hooks
  remember of one agent session (file baselines, cards sent, pending stale
  callers, Stop blocks); owner-only, dropped after `TTL`, forgotten at a
  session start with source `clear` or `compact`. Written by the daemon only.
- `workspace.json` (`crates/pixel/src/workspace_cmd.rs`): the member repositories of `pixel workspace`, canonical paths; owner-only. Written by `pixel workspace` only.
- `config.yaml` (`pixel_setup::settings::config_path`): the repository's settings,
  over `~/.pixel/config.yaml`; written by `pixel config` only, as text so comments
  stay, owner-only, never through a link. A value is the first of environment
  (`PIXEL_METRICS`, `PIXEL_DAEMON_AUTO_START`, `PIXEL_BRIEF`, `PIXEL_EDIT_HOOKS`),
  repository, global, default; every reader in the binary goes through
  `crate::settings`.
  The global file alone also holds `classify.*` and `remote_keys.*` (the opt-in
  classifier; written by `pixel config classify|classify-engine|remote-preset|remote-key`
  through `crate::classify_config` with a comment-preserving YAML edit, owner-only).

Outside the repository: the embedding model, in
`$XDG_CACHE_HOME/pixel/models/<repository with `/` as `--`>/` (`pixel_index::cache::cache_root`,
`pixel_index::semantic::embed::model_dir`), written only by `pixel search-meaning --download`
and never read without its `.complete` marker; the daemon's socket, pid and lock in `$XDG_RUNTIME_DIR`
(else `~/.cache/pixel/sockets/`), named `pixel-<xxh3 of the canonical root>`; and
`~/.pixel/setup-receipt.json` (`pixel_setup::receipt`, written by `pixel setup`
only), the original bytes of every file setup rewrote.

## Daemon and wire contract

`pixel_daemon::Service::handle(Op) -> Envelope<Value>` is the single entry point;
the socket server (`serve`) and the CLI's in-process fallback both call it.
The thread that owns the `Service` is the only writer. `plane` classifies every
op as `Read`, `Write` or `RepoWrite` with no wildcard arm; a replica answers a
read from the last published generation, and a graph read only while that
generation holds every edit batch the writer has finished, or for a `stale_ok`
read, which then carries `staleness_ms` and a `STALE_GENERATION` warning.

The service loop reads `PIXEL_DAEMON_IDLE_SECS` once: a trimmed positive `u64`
sets its idle timeout in seconds; absent, invalid or zero values use 1,800
seconds. Both the receive wait and the idle-exit check use that duration.

`Op::Brief` is a read-plane op that always goes to a read replica: the pipeline
of `pixel-brief` runs in a worker thread under its window over a `BriefEvidence`
(`pixel-daemon/src/brief/`) holding the shared stores, and the answer carries the
block and the journal record. `pixel brief` prints the block and appends the
record to `.pixel/journal.jsonl`. The op's `without` lists recipe steps to leave
out, and `PIXEL_BRIEF_STEPS=-name,+name` (`pixel_daemon::brief::STEPS_ENV`, read by
whichever process computes the brief) switches them off or on; a step also
reads tracked files through `Evidence::tracked_files` and `Evidence::read_file`
(`git ls-files`, then a capped no-follow read confined to the repository).
A pure one-definition lookup suppresses region-pack usage blocks; edits,
explicit usage/reference requests, ambiguous targets and missing definitions
retain ordinary usage packing (`chain::is_lookup_only`, `region_pack::build`).
After the render, `pixel-brief/src/probes.rs` runs the plugin host's brief probes
and merges their blocks (`PIXEL_BRIEF_PROBES=off` runs none; the daemon builds the
host from `HOME` and `PATH`).

The protocol is `pixel_proto::ENVELOPE_PROTOCOL_VERSION`; a daemon of another
version is retired and replaced by the next command, a newer protocol is left
alone. A request line is at most 64 KiB and a connection serves at most 64
requests. Every answer carries the snapshot (head, branch, dirty count, token)
and epistemics (`closed_world: false`, `lower_bound`, basis, extraction limits).

## Request path from the CLI

`client::execute` discovers the root, probes the socket with a `Ping`, sends
the op to a current daemon, starts one when none answers (unless
`PIXEL_DAEMON_AUTO_START=0`), and falls back to an in-process `Service`. Each
request leaves a `ServeStep` (route and phase timings) in the invocation's
action-log line. `review`, `call-path`, `list-signatures` and `rename` are not
wire ops: they refresh the graph in process and read it. Text output is cut at
`render::STDOUT_BYTE_CAP` at a line and says so.

## Agent integration

`pixel hook <event> --host <host> [--part K/N]` (`crates/pixel/src/hook/`) is the
one command every host's registered hook runs. `pixel_setup::Host` and `Event`
name the hosts and events, `pixel_setup::normalize` reads each host's payload
fields, and `context_envelope` / `attach_line` build the JSON each host accepts
and put the 🟩 line where it draws it (`systemMessage`, a request to repeat it,
or a `notice` for an extension). The hook reads at most `MAX_HOOK_INPUT` bytes
within `STDIN_WAIT`, asks the daemon (`Op::Brief`; auto-started, in-process
fallback) under the host's `Host::deadline` less `HOLD_BACK`, prints one
envelope or nothing, and exits 0 even after a panic or a flag it does not know
(`run::run` answers a parse error of `hook` with status 0).

| Event | What the hook does |
| --- | --- |
| `prompt-submit` | The brief: the whole block, or slice K of N of it (part 1 stores it in `brief-parts/`, later parts wait for it and for its `.done` marker). Journals the `brief` record, the `prompt` line and one `part` line each. Silent when the prompt gets none. |
| `session-start` | Forgets what the session was sent on `clear` / `compact`; warms the daemon. Prints nothing. |
| `post-tool` | Journals a `tool` line (tool, whether it wrote, repository files named). No daemon. Prints nothing. |
| `stop` | Journals the `stop` line and the turn summary; with `edit_hooks` on, asks the daemon's `StopCheck` and blocks the stop once per pending set (`pixel_setup::block_envelope`; never under `stop_hook_active`). |
| `pre-edit`, `post-edit` | Off unless `edit_hooks` is on. The files the edit tool writes and their new content come from `pixel_setup::edit_input`; each is an `EditCheck` (`phase` before or after) to a daemon that is already running, within 300 ms. Prints the `[PIXEL:EDIT]` card or the `[PIXEL:STALE-CALLERS]` note, and journals an `edit` / `stale` / `guard` line (paths and counts, with the reason for silence). No daemon: silent, and one is started for the next event. |

`post-tool` also relays: a tool result that ends with the finalized `🟩 pixel`
block of an `pixel` call has it shown to the user where the host draws one
(`pixel_setup::relay`: `systemMessage` for Claude Code and Gemini CLI, for Codex
only when one block names the result; Antigravity parks it under the
conversation's artifact directory, `hook/pending.rs`, and the `stop` hook hands
it over as an injected step). A host that keeps an `pixel` call's stderr out of
its result gets no relay.

`pixel doctor` (`pixel_setup::doctor`, driven by `crates/pixel/src/doctor_cmd.rs`)
checks what `pixel setup` wrote (`install.*`, `binary.*`, `repo.config`, read
from files) and, in the binary, `daemon.health` and the `index`, `graph` and
`history` freshness against HEAD. Each check has a stable id and a repair
command (`CHECKS`); `--fix` runs each distinct repair once and calls it fixed
only when the checks it targeted re-run green.

`pixel setup` (`pixel-setup`, driven by `crates/pixel/src/setup_cmd.rs`) writes, per
configured host: `~/.claude/settings.json`, `$CODEX_HOME/hooks.json`, Devin's
`config.json` and `~/.gemini/settings.json` hook entries
(`'<exe>' hook <event> --host <host> [--part K/N] || true`, from
`routing::hook_command`); `~/.gemini/config/hooks.json` and the Antigravity IDE
and CLI plugin directories; `plugins/pixel-brief.js` for OpenCode; the pi package
under `~/.local/share/pixel/pi-package` (`PACKAGE_DIR`) declared in pi's
`settings.json`; and `skills/pixel/SKILL.md` (`PIXEL_SKILL`, ≤ 2 KiB of body) in
every harness root. Foreign hooks and settings are kept, every rewritten file
is backed up (`<file>.pixel-bak.<nanos>-<seq>`), and the receipt
`~/.pixel/setup-receipt.json` records the original bytes so `--remove` restores
unchanged files byte for byte and keeps ones edited since. A Pixel install's
hook entries, pi package, OpenCode and Antigravity plugins and unedited
`skills/pixel` are removed by the same run (`migrate`); `~/.pixel` is left.

## Indexes and freshness

- **Text index**: a base shard at a pinned commit, a delta shard for files
  changed since, an in-memory overlay for the dirty tree. The daemon refreshes
  the overlay from watcher batches (debounced) and catches up after a HEAD move
  for at most `HEAD_CATCH_UP_MAX_PATHS` paths; a wider move leaves the covered
  HEAD where it was.
- **Graph**: `pixel_graph::refresh` runs scan, plan, extract, link and publish.
  An unchanged language is not decided again; an incremental run stores what a
  full run stores (`truth.rs`). After the first successful refresh the writer
  refreshes the graph after every batch.
- **Semantic pool** (`pixel_daemon::meaning`, feature `semantic`): the first
  `meaning` request that finds the model on disk starts one background build
  (symbol chunks from the graph, windows for other files, vectors reused from
  `code-vectors/`) and answers `warming`. A pool answers only for the
  generation it was built for; behind it, or built before the graph existed,
  the answer is `refreshing` and a rebuild starts, so a deadline-bounded caller
  (the brief) falls back to word leads. A request never downloads the model.
- **History**: ingested on demand (a bounded catch-up, then a keep-fresh loop);
  `read_only` queries never ingest.

## Testing and gates

CI (`.github/workflows/ci.yml`) runs `cargo fmt --check`, `cargo clippy
--workspace --all-targets -- -D warnings`, `cargo nextest run --workspace`,
doctests once a library crate exists, an MSRV build, `cargo deny`, the SPDX and
action-pin checks, and the website and motion builds. Four more workflows sit
beside it: `mutants.yml` (nightly, on main's cumulative unjudged diff, planned,
sharded and judged by `scripts/ci/mutants-nightly-range.py` and
`scripts/ci/mutants-gate.py`; mutation testing runs nowhere else), `fuzz.yml`
(the cargo-fuzz targets of `fuzz/`: `graph_extract` over `pixel_graph::extract::rows`
and `search_plan` against the verifier's matcher; 60 s a target on a pull request
that touches them, weekly otherwise), `codeql.yml` (code scanning; Rust only after
merge). No project board sync is configured. The CLI contract tests are
`crates/pixel/tests/cli`; `docs_drift` ties this file to `pixel --help` and the
workspace.

## Release gate

A release is a `vX.Y.Z` tag on `main` (`.agents/skills/release/SKILL.md`).
`.github/workflows/release.yml` runs, in order: `verify` (`scripts/release/check-release.py`
on `$GITHUB_REF`: `crates/pixel/Cargo.toml` carries the tag's version, `Cargo.lock`
carries every workspace member at its manifest version, `CHANGELOG.md` has the
`## [x.y.z]` heading and an empty `## [Unreleased]`; then `cargo deny check` and
the locked workspace tests), `fuzz` (`fuzz.yml`, 120 s a target), `assets`
(`release-build.yml`, the reusable workflow that builds `x86_64` and `aarch64`
musl through `cross` and `aarch64-apple-darwin` natively, packages
`pixel-<tag>-<target>.tar.gz` with its `.sha256`, a CycloneDX SBOM narrowed to the
crates the build compiles (`scripts/release/release-sbom.py`), a symbols archive
(`split-symbols.sh`), and attests every archive, SBOM and `install.sh`
with `actions/attest`, the signer being `release-build.yml`), `release`
(publishes the assets with the changelog section as the notes), and `smoke`
(installs the
published archive and `install.sh` on each target and checks
`pixel --version` names the tag and its commit). `cross-build.yml` builds the same
lanes on `main` so the tag's build restores a warm cache, and
`reproducible-build.yml` builds the `x86_64` lane twice under
`scripts/release/release-build-env.sh` and compares the bytes.
`.github/actions/setup-pixel` installs a verified release in a workflow and
prepares the repository's indexes.

## Build provenance

`crates/pixel/build.rs` emits `PIXEL_GIT_SHA`, `PIXEL_GIT_DIRTY`, `PIXEL_BUILD_COMMIT`,
`PIXEL_BUILD_TARGET`, `PIXEL_RUSTC_VERSION` and `PIXEL_BUILD_DATE` (honouring
`SOURCE_DATE_EPOCH`) for `pixel --version`, each `unknown` rather than a failed
build. The release build pins `SOURCE_DATE_EPOCH` to the tagged commit's time,
keeps symbols in the one build it makes (`CARGO_PROFILE_RELEASE_STRIP=none`, split
by `split-symbols.sh`) and remaps the checkout and `CARGO_HOME` out of the binary
(`release-build-env.sh`); `[workspace.metadata.cross]` forwards
`SOURCE_DATE_EPOCH` into `cross`'s container.
