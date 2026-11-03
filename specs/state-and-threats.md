# State on disk, and what it may not do

Status: normative for the guarantees Pixel makes about the files it writes, how
long it keeps them, and what leaves the machine. Which crate owns which file and
how each store works is ARCHITECTURE.md, "On-disk state"; the analysis of who
could attack what is `docs/threat-model.md`. This file is the short list a
change must not break.

## Where state lives

| Place | Holds | Lifetime |
|---|---|---|
| `<repo>/.pixel/` | indexes, graph, history, logs, task ledger, session files | rebuildable, except the task ledger; git-ignored through the clone's `info/exclude` |
| `~/.pixel/` | `config.yaml` (including stored remote keys), `trusted.toml`, `plugins.toml`, `plugins/`, `setup-receipt.json` | until the user removes it |
| `~/.local/share/pixel/`, `~/.local/state/pixel/` | the recall corpus and models, the Pi packages, crash-safety state of guarded git writes | see ARCHITECTURE.md |
| `$XDG_RUNTIME_DIR` or `~/.cache/pixel/sockets/` | the daemon's socket, pid and lock (`ops.md`) | the daemon's life |

Pixel's own state goes under `.pixel/`. The repository files it changes are the
ones a command was asked to change (`pixel setup --repo`, the git commands), and no
hook or brief builds an index or graph to answer (`hooks.md`).

## Writes

1. **A file Pixel writes under `.pixel/` is written without following a link**:
   a fresh temporary file renamed over the name, permissions set on the
   descriptor (`pixel_git::nofollow`); a directory is made with
   `sidecar::private_dir`, which refuses a link; a SQLite store opens with
   `SQLITE_OPEN_NOFOLLOW`. User-specific data (logs, session files, the brief
   parts, config with keys) is mode 0600 in a 0700 directory.
2. **A repository cannot hand Pixel its own `.pixel/`.** A store that opens
   (index, graph, history, the verified-history file) first runs
   `sidecar::check`: a `.pixel` that is a link, is not a directory, or holds any
   entry `git ls-files .pixel` lists is refused with the command that removes it.
3. **The brief's logs and session files never create `.pixel/`.** With no `.pixel/`
   there is nothing to record and nothing is written
   (`brief-decisions.jsonl`, `brief-feedback.jsonl`, `brief-sessions/`).
4. **A path read back from a shard or the graph is used only if it is inside the
   repository** (`pixel_git::repo_path`). A path outside it, behind a link, or
   shaped like a credential is never injected into a brief and never recorded in
   the feedback log.
5. **Names carry no host text.** A session is a file name only as the first 16 hex
   digits of the SHA-256 of the host's id, and a prompt as its SHA-256.
6. **Failure to write is not failure to work.** A log, a session file or a
   stored brief part that cannot be written is dropped; the hook still exits 0
   and prints its answer (`hooks.md`).

## What is kept, and for how long

| File | Bound | Constant |
|---|---|---|
| `actions.jsonl` | checked at start and every 50 events; past 5 MiB cut to its last 5,000 lines | `ROTATE_CHECK_EVERY`, `MAX_LOG_BYTES`, `MAX_KEPT_LINES` |
| `brief-decisions.jsonl` | the last 500 lines | `decision_log::MAX_LINES` |
| `brief-feedback.jsonl` | the last 1,500 lines | `feedback::MAX_LINES` |
| `brief-sessions/<session>.json` | a range key for 24 h, at most 1,000 keys per session; every write removes session files untouched for a day; `SessionStart` with source `clear` or `compact` deletes the file, `resume` keeps it | `sent::TTL`, `MAX_KEYS` |
| `brief-parts/<session>-<prompt>.txt` | every part-1 write removes entries older than one hour | `brief_parts::MAX_AGE` |
| `history.db` | the ingest window and budget of the facts store, eviction shrinks the file | `ARCHITECTURE.md` |

`PIXEL_BRIEF_LOG=0|false|off` writes neither brief log. Nothing switches off
`actions.jsonl` other than having no repository root; `--metrics off` only
silences the printed lines (`metrics.md`).

## What the records may contain

- **Never** the prompt text, a command, a tool output or an answer, apart from
  `typed`: the task text of a decision, with credential shapes masked and cut at
  600 characters, and a pasted block reduced to the bounded excerpt that steered
  the brief. The feedback log holds paths and counts only.
- **Arguments are logged through `logged_args`:** a credential given on the
  command line (`config remote-key <preset> <key>`, a rejected `config <key>
  <value>`) is replaced by `<redacted>`; free text given as an argument
  (`pixel classify "<text>"`, a search pattern) is recorded as typed. A new flag
  that carries a credential is masked in the action log, the metrics and every
  error message before it ships, with a test that reads the sink back
  (`change-propagation.md`).
- **Provenance** (`metrics.md`) identifies the binary and the invocation, never a
  user or a host's raw id.

## What leaves the machine

The usage logs above stay on the machine (`metrics.md`); the core binary sends
no usage data anywhere. Each of the following is the one network act of its
command, or a switch that is off until the user turns it on:

| Egress | When |
|---|---|
| a remote classifier engine receives the state text, context, labels and criteria, each capped at 32,768 characters | `pixel classify` with a remote engine, after `pixel config classify on`; the prompt brief, only with that switch on and a `weak` or `prose` prompt (`classify.md`, `brief.md`) |
| the release check: one `HEAD` request to the latest-release URL | not under `CI`, not from a hook, off with `PIXEL_NO_UPDATE_CHECK=1` |
| embedding models, downloaded from Hugging Face | `pixel recall setup`, `pixel search-meaning`, or a daemon started with `PIXEL_EMBED_DOWNLOAD=1` |
| `pixel web-search` | the user runs it |
| `pixel index-unpack <url>`, `pixel reference setup`, a plugin that declares the network | the user runs them; a network plugin must also be allowed in `~/.pixel/plugins.toml` |
| local engine setup downloads and runs the Ollaya installer | the guided classify setup, after the user accepts it |

A key for any of these resolves from the environment, then `~/.pixel/config.yaml`
(0600), then Infisical, and is never printed (`classify.md`).

## Repository content is data

A repository, a commit message, a file and a plugin manifest are inputs, never
instructions to Pixel and never trusted by their position:

- Text copied from the repository into a brief is quoted evidence between
  Pixel's own fixed lines. The closing line is a constant (`SKILL_LINE`), not
  text taken from the repository.
- A repository plugin runs only after `pixel plugin trust <name>` recorded the
  digest of its files in `~/.pixel/trusted.toml`; a changed file is untrusted
  again.
- Hostile source files are bounded by the walkers and parsers' caps; a shard or
  database that fails its bounds check is rebuilt or refused, not read on.
- A hook that fails, times out or reads garbage lets the host go on
  (`hooks.md`); the only blocks are the task gate's explicit JSON answers.

## Tests that prove it

`sidecar` tests in `pixel-git` (a linked, non-directory or tracked `.pixel` is
refused), `nofollow` tests (a planted link is not followed), `brief_feedback_cli`
and `prompt_brief_cli` (nothing created when `.pixel/` is absent, mode 0600, the
retention caps), `sent` and `brief_parts` tests (24 h and 1 h), `logged_args`
tests (masking read back from the log), `update_notice` tests (the opt-outs),
and `specs_drift` (every `pixel <name>` quoted here is a real command; the retention
numbers are not machine-checked).
