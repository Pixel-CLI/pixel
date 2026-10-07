# Pixel architecture

This document describes how the workspace is put together: the crates, the
data that lives on disk, the daemon wire contract, and the paths a command
takes from the CLI to an answer. It is the map a contributor (human or agent)
should read before touching more than one crate.

For what Pixel does and why, read `README.md`. For per-turn project rules,
read `AGENTS.md`. For the actors, trust boundaries and threats across these
crates, read [docs/threat-model.md](docs/threat-model.md).

## One-screen summary

```text
 agent CLI (Claude, Codex, Cursor, pi, …)
   │  agent prompt + hooks / extensions     (pixel-install)
   ▼
 pixel binary (crates/pixel)  ── clap commands, prints text or --json
   │  Request = pixel_proto::Op   (NDJSON over a Unix socket)
   ▼
 pixel-daemon ── Service::handle(Op) -> Envelope<Value>
   │  in-process fallback when no daemon is reachable
   ├── pixel-index    trigram text index          .pixel/ shards
   ├── pixel-graph    symbols / imports / calls   .pixel/graph.v2.db
   ├── pixel-facts    history facts + diffs       .pixel/history.db (lazy: built on first history query)
   ├── pixel-rank     task -> ranked file list    (scoring pure; signals read git + sniper)
   ├── pixel-context  token-budgeted rendering    (pure)
   ├── pixel-ops      guarded git mutations       .pixel/journal, snapshots
   ├── pixel-recall   transcript corpus + embeddings (machine-wide; lazy: ingested on first recall query)
   └── pixel-git      the only git subprocess wrapper
```

Everything below the CLI is a library crate. Only `crates/pixel` builds a
binary, and Pixel is deliberately a CLI plus hooks and extensions, not an
MCP server. `pixel install` registers no MCP server with any agent.

## Crates

| Crate | Role | Depends on (pixel crates) |
| --- | --- | --- |
| `pixel-cli` (bin `pixel`, in `crates/pixel`) | Command-line surface. Parses argv with clap, talks to the daemon or runs the service in-process, prints text or JSON. Also hosts the hook entry point under `run-hook` (alias `hook`: `task-event`), `rescue`, `recall`, and `sniper` sub-commands. | every library crate except `pixel-context` (reached through the daemon) and `pixel-bench` |
| `pixel-proto` | The shared contract crate: `Envelope`, `PixelError` and `ErrorCode`, `Epistemics`, `SnapshotInfo`, `Budget`, `Warning`, the `Op` request enum, and `commands::RENAMED_COMMANDS`, the old-to-new CLI subcommand names the CLI accepts as hidden aliases until 1.0. No I/O, no business logic. Every other crate that speaks the wire format depends on it, and it depends on nothing internal. | none |
| `pixel-daemon` | Transport-agnostic `Service` (`api.rs`) and the Unix-socket NDJSON daemon with filesystem watching (`daemon.rs`). Dispatches each `Op` to the right library, attaches snapshot and epistemics metadata, and is the one place a retrieval envelope is built. Also hosts the recall daemon service. | index, graph, context, rank, proto, ops, facts, session, recall, git |
| `pixel-index` | Sparse n-gram (trigram) text index: gram extraction, window weighting, posting-list algebra, git-anchored base and delta shards, working-tree overlay, query planner, verification, and the `gitsync` helpers that read HEAD, branch, and porcelain status. | git |
| `pixel-graph` | Code graph: tree-sitter extraction of symbols, imports, and call sites per file; import resolution; tiered call resolution with an epistemic envelope; and the analyses `impact`, `trace`, `process`, `cluster`, `changes`, `targets`. `store` owns the SQLite schema. | git, index |
| `pixel-facts` | History-wide fact and diff ingest, search, lifecycle, and rescue discovery. Owns `history.db`: commit metadata, diff text, and contentless FTS5 trigram indexes over the diffs and paths. Demand-driven: the daemon never ingests at startup, and `status` and `doctor` never create the db — the first history query runs a bounded catch-up (`PIXEL_FACTS_QUERY_BUDGET_MS`, default 3 s) and spawns the low-priority keep-fresh loop that never blocks queries. Bounded: diff text is kept for the last 365 days (`PIXEL_HISTORY_WINDOW_DAYS`, `0` for all) within 256 MiB of used pages (`PIXEL_HISTORY_BUDGET_MB`), newest first; older diffs are evicted, metadata never is. `build-index --history` remains the explicit full build. Backs `excavate`, `lifecycle`, `history-search` and `rescue` discovery (`resolve` is the graph's concept index). | git |
| `pixel-rank` | Fusion core for `targets` and ranked `search`: task text and signal inputs in, closed prioritized P0/P1/P2 file list out. The scoring is pure; `compute_signals` gathers the activity channel itself (git log churn when facts have none, and a failed or capped scan is reported as unavailable rather than as an empty map) and takes the session and error-sink channels from its caller — the daemon feeds neither of those. | graph, git, session |
| `pixel-context` | Semantic compression of code-context items: layered renderings that fit a token budget instead of raw source dumps. | none |
| `pixel-ops` | Safe git mutation infrastructure ported from usable-git: snapshot store, repository lock, operation journal, recovery keys. Implements `inspect`, `review`, `history`, `diff`, `publish`, `push`, `ship`, `branch`, `update`, `sync`, `reconcile`, `rewrite`, `provenance`, `branches`, `env`. | git |
| `pixel-git` | The single git subprocess wrapper for the workspace. Replaced three earlier ad-hoc wrappers. Any crate that shells out to git goes through `GitRunner` (timeout, output cap, redacted stderr); `crates/pixel-git/tests/boundary.rs` fails the build on a `Command::new("git")` in any other crate's non-test code. Also owns the trust boundary of `.pixel/`: `sidecar` refuses a `.pixel` that is a link or that git tracks and creates owner-only directories without following a link, `nofollow` opens files without following a link at their name, and `repo_path` confines a stored path to the repository root. | none |
| `pixel-recall` | Machine-wide LLM transcript retrieval: ingests Claude Code, Codex, opencode, pi, Devin, Cursor, zcode, and Gemini transcript stores into one SQLite corpus, then serves lexical and semantic search. Demand-driven: nothing scans transcripts until a recall query runs — the in-process path then catches up per agent (last week cold, since-last-ingest warm, capped at 30 days; `recall index` for the full history). Owns the embedding backends (`fastembed` ONNX and pure-Rust `model2vec`, both behind features) and the `search-meaning` code chunker, which reuses the graph's tree-sitter extraction. | git, graph, index, rank |
| `pixel-session` | One-look error capture: every error from every layer lands at throw time in one structured local SQLite sink, queryable in one call. | git |
| `pixel-actionlog` | Append-only local JSONL invocation records: measured command/outcome/duration/output volume plus versioned workflow estimates; backwards-compatible `pixel action-log` and `pixel token-savings` reporting. | git |
| `pixel-task` | Durable completion contracts, deterministic workflow gates, source manifests, private verification receipts, measured task trajectories, pure policy replay, and explicit controlled evaluation. | git, ops |
| `pixel-release` | `pixel check-release`: the consistency checks a release tag must pass (CLI version, `Cargo.lock` freshness, changelog cut). Pure functions over file contents. | none |
| `pixel-flow` | Deterministic browser and configuration flow replay: save, get, list, revise, replay, delete proven agent-browser paths. Flows live under `~/.local/share/pixel/flows/`. | none |
| `pixel-install` | Idempotent `pixel install`, `pixel uninstall`, `pixel doctor`. Global install: Claude and Codex task lifecycle hooks, cleanup of their retired retrieval hooks and prompts and the `claude()` shell wrapper, removal of the Pi `APPEND_SYSTEM.md` retrieval block, and the explicit Pi `/pixel-impact` package (`~/.local/share/pixel/pi-package/`, declared in Pi's `settings.json`) when Pi is present. No prompt is deployed and no other host is wired: the retired OpenCode, Devin, Antigravity, zcode, Cursor and Copilot CLI artifacts and the deployed agent prompts are removed. An accepted classify engine (interactive install or `pixel config setup`) proposes the optional helpers: the `pixel-classify` skill into every configured harness's skills dir (`skills/pixel-classify/` under `~/.claude`, `~/.codex`, `~/.cursor`, `~/.devin`, `~/.gemini`, `~/.pi/agent`, `~/.config/opencode`) and, when Pi is present, the `pixel-classify-files` tools as a second local Pi package (`~/.local/share/pixel/pi-classify/`, declared in Pi's `settings.json`). `--repo`: native-default migration for Claude and Codex, and removal of the retired Devin and Pi project guards and `AGENTS.md` retrieval blocks (see "Agent integration"). Backs up changed files (`<file>.pixel-bak.<nanos>-<seq>` beside each); `uninstall` keeps those backups (a user's `skills/pixel-classify/` is renamed to `<harness root>/pixel-classify.pixel-bak.<nanos>-<seq>`, outside `skills/` so the harness stops loading it) and ends on a `backups` step listing them with the quoted `rm --` (`rm -rf --` for directories) command that drops them. A run at a terminal opens with the `intro` animation (frames only; `pixel` owns the tty) and ends on the `banner` summary. | proto, daemon, index, facts, git |
| `pixel-ultraflow` | The classify-driven browser loop over saved flows: the observation (`agent-browser snapshot -i` parsed into numbered slots, `elements`), the indexed action space of operation-target pairs (`action`), the decision seam (`decide`), the discovery loop and its single-cycle unit (`discover`), the composition of what worked into a `pixel-flow` document whose `conditional` steps carry the conditions that tell its branches apart (`compose`), and the replay that decides those conditions with `pixel classify` and re-decides a step whose page moved on (`replay`). Drives `pixel-flow`'s browser seam; the engine is a trait, so the whole loop is tested without a model, a network or a page. | flow |
| `pixel-bench` | Criterion benches and a real-source corpus builder (gram extraction, latency, NDCG relevance). Not shipped. | index (dev: daemon, graph, proto, recall) |

Dependency rule: `pixel-proto` and `pixel-git` are leaves (so are `pixel-context`, `pixel-flow`, `pixel-ultraflow` (which depends on `pixel-flow` alone) and `pixel-release`; `pixel-session` and `pixel-actionlog` depend on `pixel-git` only). `pixel-daemon` is
the integration point and is the only library crate allowed to depend on
almost everything. The CLI depends on the daemon plus whatever it needs for
commands that never touch the daemon (install, flow, actionlog, release-check).

## Command surface

Every subcommand of the built binary, one line each, in `pixel --help`
order. `crates/pixel/tests/cli/docs_drift.rs` fails when a command listed
by `--help` is missing here, or when any `` `pixel <name>` `` in README,
ARCHITECTURE, CONTRIBUTING, `docs/manual-setup.md`, the site's `website/content/docs.md` and `benchmarks.md`, or the bundled agent prompts
(`crates/pixel-install/assets/pixel-agent-prompt.md`,
`pixel-subagent-prompt.md`) names a command the binary does not have. The 45 names renamed after 0.2.4 still parse as hidden aliases until 1.0 (table in `pixel_proto::commands::RENAMED_COMMANDS`, README "Renamed commands"); `--help` and this table list only the current names.

| Command | Does |
| --- | --- |
| `pixel build-index` | Build (or rebuild) the text index for a directory tree |
| `pixel search-content` | Search the indexed tree with a regex pattern. |
| `pixel search-like-rg` | Native-output literal file search for automatic routing; unsupported inputs execute the original rg/grep command without modification |
| `pixel run-recipe` | Compile and execute one bounded deterministic retrieval recipe |
| `pixel search-meaning` | Semantic code search: embed a natural-language question ("how is authentication handled?") and rank files by fusing semantic and BM25 lexical ranks over symbol chunks (tree-sitter symbols with their doc comments, windows for unparsed files); tests, configuration/data and docs are weighted below code unless the question names them. |
| `pixel scope-task` | Sniper target list: task description in, closed prioritized file list out (P0 = start here, P1 = likely, P2 = droppable). `--read-only` instead returns a typed deterministic fact envelope only from a compatible running daemon; it never starts a daemon, builds/refreshes indexes, or writes the targets manifest. |
| `pixel brief` | Print the bounded `[PIXEL:BRIEF]` evidence context for a code-related prompt in an indexed repository; optional `[path]` selects the repository, and no applicable brief prints nothing. |
| `pixel execution-brief` | Build a task-specific ordered retrieval route from scope-task evidence: one populated first command, one retry for an empty or irrelevant result, a bounded native fallback, a maximum 40-line read, and minimal validation. |
| `pixel plan-rollback` | Surgical revert planner: locate the files a problem points at, list recent versions with the likely-breaking commit flagged, recommend a last-known-good candidate. |
| `pixel ai-cli-readify` | Provider readiness for the four agent CLIs (Codex, Claude Code, Antigravity, Devin): probe Ollama Cloud, then probe each agent in its own lane, reporting an agent blocked when the provider did not answer; `--apply` points the agents' configs at the provider that answered; `--approve --workspace <dir>` clears the named folder's startup gate through Codex's own `config/batchWrite` RPC and Claude's `~/.claude.json`. |
| `pixel find-symbol` | Look up symbols by name in the code graph |
| `pixel list-signatures` | All signatures in a file — the skeleton view at ~10% of Read cost |
| `pixel note` | Human notes on the map: durable annotations keyed by file + symbol name (or concept norm). |
| `pixel repo-map` | Structural repo map: every indexed file with its symbols. |
| `pixel pack-context` | Budget-fitted context for a symbol uid |
| `pixel impact` | Blast radius of a symbol (callers upstream / callees downstream); `--no-refresh` bounds a query against an existing fresh graph without index maintenance |
| `pixel who-calls` | Direct callers or callees of a symbol |
| `pixel rename` | IDE-style symbol rename: graph-resolved definition, call, reference, and import sites, each verified against a fresh tree-sitter parse before writing; unresolved same-name sites are reported, never guessed. `--dry-run` prints the edit set without touching files |
| `pixel call-path` | Call path between two symbols; its `successor` field names the `pixel evaluate path` command that asks the same question with a bounded answer |
| `pixel evaluate` | Bounded predicate evaluation with a witness: does a path exist between two symbols in the indexed call graph, with the snapshot the answer is about, an exhaustive-traversal absence, or a typed reason for not answering |
| `pixel cycles` | Bounded recursion-cycle enumeration with witnesses and explicit coverage: finds strongly connected components in the call graph (potential recursion cycles) over `Calls` edges only — `HasMethod` ownership is excluded because ownership is not runtime invocation. Bounded by node, edge, time, and component budgets; each reported cycle carries a concrete closed-path witness re-read from the store. The coverage report states whether the enumeration was exhaustive or which budget stopped it, so an incomplete graph is never read as proof of safety |
| `pixel list-flows` | Discovered execution flows |
| `pixel list-areas` | Functional-area clusters |
| `pixel what-changed` | Symbols/flows affected by working-tree changes |
| `pixel rebuild-graph` | Force (re)build of the code graph db |
| `pixel workspace` | Multi-repo registry (`.pixel/workspace.json` members add/remove/list); `--workspace` fans `impact`/`who-calls` out across registered repos with per-repo provenance |
| `pixel index-pack` | Freeze the index into one checksummed `.pxpack` bundle — CI builds once, teammates install instead of re-indexing |
| `pixel index-unpack` | Install a packed index bundle from a path or https:// URL, hash-verified, refusing to overwrite a live index without `--force` |
| `pixel reference` | Manage version-pinned reference corpora (`.pixel/reference.json`) for multi-repo analysis on top of workspaces and index packs |
| `pixel status` | Index + graph freshness status |
| `pixel coverage` | Per-language index coverage: files on disk vs files indexed, with unrecognized extensions surfaced — the "what did the index miss?" answer |
| `pixel audit` | What an agent reads to learn what the largest source files contain: each whole file against its `list-signatures` outline in tokens (bytes / 4, floored), the total and the per-file median, files changed since indexing or with no signatures left out and counted, then per-language coverage. Builds the graph on a first run only; local, read-only, sends nothing |
| `pixel space` | Audit how much disk the pixel index (`.pixel/`) takes across every project under this tree: per-project shard size plus the accumulated total (`--json` for structured output), and remove the rebuildable shards with `--delete` (one confirmation, or `--yes`). Local and read-only until `--delete` |
| `pixel prepare-repo` | Make a repository ready for agent work: index, graph, and warm daemon |
| `pixel index-stats` | Show raw shard metadata (legacy) |
| `pixel daemon` | Manage the per-root background daemon |
| `pixel recall` | Search and browse LLM CLI transcripts (machine-wide corpus) |
| `pixel list-errors` | One-look error capture: query the sniper error sink |
| `pixel classify` | Zero-shot decision over a bounded label set. Engines: `remote` (default; an OpenAI-compatible chat completion — `--remote-preset openrouter\|ollama\|local\|deepseek\|opencode-go\|jev` picks the endpoint and key variable, `--remote-model` the model; probabilities are verbalized, renormalized to sum 1; `jev` instead serves TypeSafe's hosted Jev decision model over its `/v1/systemone` wire with native calibrated probabilities, disclosed in `snapshot.basis`) or `ollaya` (`--engine ollaya --ollaya-url …`: a local Ollaya decision daemon's native typed-choice readout, TypeSafe-compatible `/v1/systemone`, with calibrated confidence disclosed in the snapshot). Remote keys resolve in disclosure order: the preset's env var, then `pixel config remote-key <preset>`, then an Infisical project when `INFISICAL_TOKEN` and `PIXEL_INFISICAL_PROJECT_ID` are set (self-hosted via `PIXEL_INFISICAL_URL`; read once per invocation, never stored in the Pixel config). Without `--engine` the stored preference decides (`pixel config classify-engine local\|remote\|jev\|auto`, or `pixel config remote-preset <preset> [--model --base]` for the provider, model and endpoint; `pixel install` proposes Local/Remote/Jev with each option's accuracy — Jev takes an OpenCode Go or TypeSafe key and offers that source's model pair), probing the local daemon and falling back to remote. Model-engine output discloses `snapshot.deterministic=false` and `snapshot.provider`; a `--history` accept instead names the verified-history tier with `snapshot.deterministic=true` and no provider, since a verified match is not a model output. `--context` keeps shared framing out of the state. State, context and criteria are capped separately with disclosure. Without `--label`, the local engine answers the default question battery (Ollaya's `triage` preset: `choice`/`score`/`noul` typed answers) instead of a single decision — the remote engine still requires labels. No pixel daemon; `--jsonl` serves one decision per stdin line. `--task-intent` judges the text with the built-in coding-task labels (bugfix, feature, refactor, investigate, question, review, ops) and adds the fitting pixel ops (`next:` / `next_ops`); `--if-warm` answers only from a local engine already listening — never starts it, never falls back to remote — and otherwise exits 1 with empty stdout. `--debug` asks every configured engine (local, the remote chat preset and Jev) the same labeled spec in parallel and reports each one's probabilities or its error row; `--remote-model` and `PIXEL_REMOTE_*` reach only the selected preset's lane, so a Jev lane beside another preset resolves Jev's own key, base and model. The legacy/manual Claude prompt hook asks the same `--task-intent` question itself (local daemon warm, `classify.enabled` on, 300 ms cap) and adds the verdict to the `[PIXEL:TASK_RUNTIME v1]` packet as a classifier claim |
| `pixel classify-eval` | Offline go/no-go evaluation of the verified-history retrieval tier against frozen baselines. Reports per-label precision/recall, macro scores, confusion matrix, accepted-coverage vs error curves with Wilson confidence intervals, and system metrics (fallback rate, model calls avoided, measured tier and modeled end-to-end latency, storage, error rate). The evaluation never calls a model, so the fallback path is modeled from the `--model-error-rate`/`--model-latency-ms` assumptions and disclosed as such in the report's `epistemics`/`snapshot` envelope. A pre-registered error bound and minimum useful coverage determine the verdict, and a tier that meets those bounds is still held to no-go when the strongest frozen baseline already answers with lower error at equal or better coverage |
| `pixel classify-history` | Manage the verified-history store: `list`, `add`, `remove`, `correct`, `clear`. Only human/independently verified labels are stored; model predictions cannot self-certify |
| `pixel web-search` | Deterministic web lookup for terms the index cannot know — the refine step of a gated `pixel plan`. SearXNG alone when `PIXEL_WEB_SEARCH_URL` is set; otherwise DuckDuckGo, then Wikipedia while the hits are fewer than `--limit`. No LLM, no daemon |
| `pixel repo-state` | Show repo state: HEAD, branch, dirty files, fingerprints; `--include-clean` adds the capped tracked-clean list |
| `pixel review-changes` | Review working-tree changes (staged, unstaged, untracked, conflicted) |
| `pixel review-gate` | Deterministic pre-review: `what-changed` plus the mechanical rules — credential-shaped added lines, and changed symbols whose callers were not themselves changed — each finding carrying its witness; the caps it fired ride the epistemics envelope |
| `pixel commit-history` | Commit history with detail levels and byte caps |
| `pixel diff` | Structured diff between two refs (or ref → working tree) |
| `pixel commit` | Stage files, commit, and optionally push (crash-safe, idempotent) |
| `pixel push` | Leased push to a remote (crash-safe, idempotent) |
| `pixel commit-and-push` | Publish + push in one op (commit then leased push) |
| `pixel new-branch` | Create a new branch from HEAD (or --from <ref>) |
| `pixel fast-forward` | Fast-forward merge to a target OID (refuses non-ff + dirty intersection) |
| `pixel fetch` | Fetch from a remote (idempotent) |
| `pixel find-code` | Engine 1: resolve a phrase to code via the concept index |
| `pixel search-history` | M3: history-wide fact + diff search |
| `pixel file-history` | Engine 2: lifecycle of a path or token |
| `pixel dig-history` | Engine 2: history-wide discovery (rescue v2) |
| `pixel sync-branch` | Engine 4: one-call deterministic branch sync |
| `pixel record-event` | M5: journal a session event (fire-and-forget) |
| `pixel install` | Idempotently install Claude and Codex task lifecycle hooks, remove retired retrieval registrations and the Claude shell wrapper, install Pi's explicit impact extension when configured, and retain the existing profiles for other configured hosts |
| `pixel uninstall` | Remove everything `pixel install` wrote: managed blocks from agent-config files, hook entries from all settings files, hook scripts, the pi guard extension, the rule source file, and the pixel binary itself. |
| `pixel check-release` | Check that a release tag is consistent with the tree before anything is built or published: crates/pixel/Cargo.toml carries the version, Cargo.lock is fresh for every workspace member, CHANGELOG.md has the `## [x.y.z]` heading and an empty Unreleased section. |
| `pixel self-update` | Rebuild the binary, stop the daemon, copy the new binary to the install path, and optionally restart the daemon. |
| `pixel doctor` | Health check: install state, daemon, index/graph/facts freshness |
| `pixel run-hook` | Hook entry point `task-event`, invoked by the Claude and Codex task lifecycle hooks; the retired verbs exit 0 silently |
| `pixel config` | Show effective settings and their sources; `setup` offers guided terminal configuration (also offered on interactive global install); `classify on\|off` controls the global classify kill switch (disabled by default); `classify-engine` stores the engine preference (`local\|remote\|jev\|auto`), `remote-preset <preset> [--model --base]` the provider it runs, `remote-key <preset>` its key; `edit [--repo]` opens commented YAML in `$VISUAL`/`$EDITOR`. Global `~/.pixel/config.yaml`, repository `.pixel/config.yaml`; legacy JSON remains supported. |
| `pixel task-state` | Alias `task`: begin, contract, prepare, verify, review, finish, status, events, route, replay, evaluate, cancel and recover a durable task; show/reset retain the Claude packet interface |
| `pixel action-log` | Self-assessment: pixel's own action log (what ran, what went wrong). |
| `pixel token-savings` | Token-savings report: for retrieval-shaped commands (search/query/ context/resolve) that recorded snippet-vs-pool volumes, aggregate the fraction of the candidate pool the agent did NOT have to read. |
| `pixel squash-branch` | Squash every commit on the current branch since its base into ONE commit (crash-safe, backup-ref'd), optionally force-pushing with lease |
| `pixel who-wrote` | Per-region blame attribution: who introduced/owns each region of a file |
| `pixel list-branches` | One-call read-only branch inventory: ahead/behind, merged, stale, unpushed — the deterministic "did you push everything?" answer |
| `pixel edit-env` | Additive-only, key-level .env mutations with snapshots and restore. |
| `pixel plan` | Deterministic todo list generation from code analysis; emits blocking verification gates (auth session, env keys, provider keys, real DB state) detected in the plan's file set; persists findings in `.pixel/plan.json` so `--status`/`--done N`/`--undone N`/`--prune` track execution state across re-plans without the daemon |
| `pixel ultraflow` | The classify-driven browser loop over saved flows. `discover --url --goal` asks `pixel classify` one question per cycle whose options are the operation-target pairs the page currently offers, so one answer is one executable action; a typed field is filled from a declared `--var` or from a string the goal itself contains, never from an invented value. What worked composes into a `pixel flow` document under the flow store (`--save`), and `--repeat N` records a `conditional` where two runs diverged, worded so the branch is a page condition rather than a position. `replay <name>` follows that document, deciding each `conditional` and the flow's success signal with `pixel classify` (the text matcher is the disclosed fallback), and re-decides a step whose page no longer matches — `--update` records the new branch into the flow, so the next replay is deterministic where this one had to think. Drives `agent-browser --session comet`, the session `pixel flow run` uses. Each question is bounded by the *engine's* option budget, not the schema ceiling (`Decider::option_budget`: the local `winnow:e4b` accepts 64 labels, measured — a page with more controls has its tail reported in the trace rather than dropped quietly) |
| `pixel flow` | Save, retrieve, list, revise, run, and replay proven agent-browser paths (auth flows, config flows) so the agent follows a deterministic shortcut instead of re-discovering the UI from scratch every time. `run <name>` drives agent-browser with the plain executor; for a classify-decided replay with repair, use `pixel ultraflow replay` |
| `pixel help` | Print this message or the help of the given subcommand(s). |

## On-disk state

Per repository, under `.pixel/` (git-ignored):

| Path | Owner | Contents |
| --- | --- | --- |
| `base.shard`, `delta.shard`, `state.json`, `build.lock` | `pixel-index` | Base shard for all tracked files at a pinned commit, delta shard for files changed between that commit and HEAD, and `state.json` as the delta-layer sidecar (tombstones for superseded base paths). The dirty working-tree overlay is in memory only. First process to hold `build.lock` builds; others wait. |
| `graph.v2.db` | `pixel-graph` | SQLite: files, symbols, edges with resolution tier. Built lazily on first graph command. The name moves with the schema (`pixel_daemon::api::GRAPH_DB_FILE`); user-facing messages still say `graph.db`. |
| `history.db` (+ `-wal`, `-shm`, `history.db.lock`) | `pixel-facts` | SQLite: commit facts, diff text, lifecycle, FTS5 trigram indexes. Populated by `pixel build-index --history` or the daemon ingest thread on the first history query; capped by the window and budget above, with `auto_vacuum = INCREMENTAL` so an eviction shrinks the file. Schema version `FACTS_SCHEMA_VERSION` (3) in `PRAGMA user_version`: another version is rebuilt, except 2 (`UPGRADES_IN_PLACE_FROM`), whose dates are repaired in place. |
| `code-vectors/` (`manifest.json`, `seg-*.vec`, `lock`) | `pixel-recall` | `search-meaning` chunk vectors, keyed by the xxh3-128 hash of the chunk text seeded with the model id, embedder revision and `CHUNKER_VERSION`; stored as the model's `f32`s. Written only when the search root carries `base.shard` and is not `$HOME`, never by the daemon's semantic fallback. `flock` on `lock` (shared to read, exclusive to write), segments immutable, the manifest replaced by rename; rewritten with the live rows once unreachable ones exceed a quarter of them. |
| `actions.jsonl` | `pixel-actionlog` | One line per invocation, with the route and phase timings of each request it served (`serve`). |
| `reconcile-conflict.json`, `env-snapshots/` | `pixel-ops` | Conflict marker `reconcile` writes and clears (no hook reads it), and the pre-mutation copies `env` takes. |
| `calls.json` | CLI | Circuit breaker counters for repeated identical calls. |
| `task-runtime.json` | CLI `task-state show/reset` and Claude hooks | Existing bounded Claude context packets; independent of completion evidence. |
| `tasks/<id>/journal.jsonl`, `tasks/<id>/task.json`, `tasks/<id>/lock`, `tasks/<id>/run-<run_id>.json` | `pixel-task` | Authoritative checksummed task transactions and a rebuildable view, serialized by task lock; verification leases record child ownership for interruption recovery. Legacy v1 records migrate as unverified. |
| `tasks/source-manifests/<content_id>.json`, `tasks/source-cache.json`, `tasks/session-locks/`, `tasks/session-context/`, `tasks/route-locks/`, `tasks/enforced-sessions/` | `pixel-task` and task bridge | Immutable source manifests, metadata digest cache, atomic provider/session task binding, bounded latest prompts for first-edit activation, per-decision classification locks, and one empty marker per provider/session whose task recorded enforced gates: when the ledger cannot answer within the hook deadline, only a marked session or `task.enforcement: enforce` keeps edits and completion denied. |
| `task-hook-observations.json`, `task-hook-observations.lock` | CLI task bridge | Last real host invocation per provider; doctor reports observed activity separately from installation and trust. |
| `plan.json` | CLI `plan` | The persisted `pixel plan` checklist, so `--status`/`--done`/`--prune` survive a re-plan. |
| `workspace.json` | CLI `workspace` | The registered member repositories. |
| `config.yaml` (legacy `config.json`) | CLI `config` | Repository-level settings over `~/.pixel/config.yaml` (`pixel config edit --repo`). |

A repository can commit `.pixel/`, links included, so nothing in it is
trusted by name. The index, graph and history stores, and `index unpack`,
call `pixel_git::sidecar::check` when they open: a `.pixel` that is a link,
or that holds anything `git ls-files .pixel` lists, is refused with the
command to delete it (the integrity markers inside the files tell stale from
current, not forged from genuine). Every file pixel writes there goes through
`pixel_git::nofollow` (`O_NOFOLLOW`, a fresh temporary file renamed over the
name, permissions set on the descriptor) and every directory through
`pixel_git::sidecar::private_dir`, which refuses a link; the SQLite stores
open with `SQLITE_OPEN_NOFOLLOW`. Paths read back from a shard or the graph
are used only when `pixel_git::repo_path` finds them inside the root.

Machine-wide:

- `~/.local/share/pixel/flows/`: saved flows (`pixel-flow`, `$PIXEL_FLOW_DIR`
  overrides).
- `~/.local/share/pixel/pi-package/`: the Pi package `pixel install` writes
  (`package.json`, `extensions/pixel-impact.ts`) and declares in the
  `packages` list of Pi's `settings.json`; `pixel uninstall` removes both.
- `~/.local/share/pixel/pi-classify/`: the optional classify tools'
  Pi package, written and declared like the one above only when the
  classify helpers are accepted; `pixel uninstall` removes both.
- `~/.local/share/pixel/recall/` and `~/.local/share/pixel/models/`: the
  recall corpus (`$PIXEL_RECALL_DIR` overrides) and the embedding models,
  downloaded once from Hugging Face on `pixel recall setup`.
- `~/.local/state/pixel/sniper/<project key>/`: the `pixel-session` error sink
  (`errors-v1.sqlite` plus a `project.json` naming the root);
  `$PIXEL_SNIPER_STATE_ROOT` overrides the state root.
- `~/.local/state/pixel/` (`$XDG_STATE_HOME/pixel`): `pixel-ops` crash-safety
  state, keyed by a sha256 of the repository: `journals/`, `snapshots/`,
  `publish-recovery/` and `locks/<hash>.lock/owner.json` (the lock alone is
  keyed by the canonical git common directory, so every worktree shares it). Guarded git
  mutations write nothing under `.pixel/` except the two entries above.
- `~/.local/state/pixel/pr-swarm/`: the repo-local `pr-swarm` reconciler's
  state — `reconcile.log`, `lock/`, `resolve.tsv`, `last-run.jsonl` and
  `watch.pid` (the `watch` loop's claim, the authority on whether one runs) —
  written by `scripts/pr-swarm.sh`, not by any `pixel` crate
  (`.agents/rules/pr-swarm.md`). The pane set itself is derived from rmux
  pane titles, not from these files.
- Daemon socket and pid: `$TMPDIR` on macOS, `$XDG_RUNTIME_DIR` on Linux
  (else `~/.cache/pixel/sockets/`), named
  `pixel-<xxh3 of canonical repo path>.sock`, `.pid` and `.lock`.

## Daemon and wire contract

`pixel-daemon` exposes one function that matters: `Service::handle(Op) ->
Envelope<Value>`. The Unix-socket daemon reads one JSON `Op` per line and
writes one JSON `Envelope` line back. Request handling is single-threaded; an
accept thread and a `notify` watcher feed one channel. The watcher debounces
filesystem events and refreshes the index and graph for changed files. It is
registered on its own thread so the daemon answers from its first request: a
recursive watch walks every directory under the root, ignored ones included,
and took 11 to 28 s over a repository's 11 515 `node_modules` directories.
Once it is live, `Corpus::watch_ready` re-reads every path `git status`
lists, every path the index overlay held (an edit discarded meanwhile leaves
`git status` clean) and every path a HEAD move since the open changed, since
edits made meanwhile raised no event. The daemon exits after
thirty minutes idle.

Two version numbers exist and must not be conflated:

- `pixel_proto::ENVELOPE_PROTOCOL_VERSION`: the envelope schema (`protocol`
  field), currently 1.
- `pixel_daemon::api::PROTOCOL_VERSION`: the socket request/response format.
  Bump it when an older daemon process could not safely serve a newer CLI.
  The CLI pings first and compares.

The envelope:

```json
{
  "ok": true,
  "op": "search",
  "protocol": 1,
  "requestId": "…",          // optional
  "snapshot":   { "head": "…", "branch": "…", "dirty_count": 0 },   // `dirty: [paths]` on inspect/review only
  "epistemics": { "closed_world": false, "lower_bound": true, "staleness_ms": 0, "basis": "…" },
  "budget":     { "byteCap": 262144, "used": 0, "truncated": false },   // + `cursor` when paged
  "result":     { … },        // present when ok
  "error":      { "code": "NOT_FOUND", "message": "…" },   // present when !ok
  "warnings":   []
}
```

`requestId` and `budget` are part of the schema but no response is built
with them yet, so their absence means "not reported". The caps that bite
today report themselves inside the op's own result (`byte_cap`,
`truncated`, `next_cursor`, `next_offset`) or as envelope `warnings`.
`error.code` is the code the daemon classified the message into; the
variants no producer reaches are listed on `pixel_proto::ErrorCode`.

Invariants enforced by `Service::handle`:

- Success carries `result`, failure carries `error`. Never both.
- Every retrieval op (`search`, `resolve`, `targets`, `impact`, `uses`,
  `trace`, `changes`, `context`, `symbol`, `processes`, `clusters`, `plan`) gets an
  `epistemics` object. Ops that hit a cap name it in `basis` and mirror it as
  a warning. Ops that attested nothing get a conservative not-closed-world
  default instead of an implied claim of completeness. `search` also returns
  each cap that fired as `cap_hits` (`{kind, text}`, kinds `byte_cap`,
  `row_limit`, `ranked_pool`, `credential_hidden`), so a reader that already
  stated a bound drops it by kind rather than by matching its sentence. It
  also takes the `-g`/`-t` rules (`globs`, `types`) and drops the files they
  exclude before paging (`pixel_index::path_filter`), so `limit`, `offset`
  and `truncated` count kept matches.
- Retrieval ops and git-state ops (`inspect`, `review`, `diff`, `status`,
  `changes`) get a `snapshot` so the caller can correlate the answer with the
  working tree it was computed against. Only `inspect` and `review` carry the
  `dirty` path list; every other op ships `dirty_count` instead
  (`SnapshotInfo::compact`), so an untracked `vendor/bundle` of 15 000 paths
  does not inflate a `symbol` answer to 240 KB.

Adding an op is one variant on `pixel_proto::Op`, one arm in
`Service::dispatch` and one entry in `pixel_proto::SESSION_CAPABILITIES`.
Unit tests in `pixel-proto` check both: `op_name_matches_serde_tag` (the
`Op::op_name` of every variant is its serde tag) and
`session_capabilities_track_every_real_op` (the capability list and the enum
agree).

## Request path from the CLI

1. `main.rs` parses argv with clap. Commands that need the repository call
   `execute(path, Op, no_daemon)`. A command may compose several ops:
   `run-recipe --kind locate` calls `execute` once per op (resolve, a context
   per target, callers, file targets on a miss), never a CLI subprocess, so
   the daemon and in-process paths give the same answer; it compares the
   responses' snapshots and states a mismatch as a limit.
2. `execute` discovers the repo root, then tries the daemon: connect to the
   socket, ping (5 s timeout), and send the op. If no daemon answers, or one
   answers on an older `PROTOCOL_VERSION` (it is shut down first), it spawns
   `pixel daemon start <root> --foreground` in the background and polls the
   socket for up to five seconds. A daemon on a newer protocol is left alone
   and the command runs in process. `PIXEL_DAEMON_AUTO_START=0`, or
   `daemon_auto_start: false` in the config, disables the spawn.
3. If the daemon path fails, the CLI opens `Service` in-process and calls
   `handle` directly. Both paths return the same `Envelope`.
4. `unwrap_response` turns a failure envelope into an `Err(message)` that
   `main` prints to stderr with exit code 1. Under `--json` the CLI also
   answers on stdout with the failure envelope (`ok: false`, `error.code`,
   the same message) — classified by the daemon's `failure_response`, so a
   CLI-side failure carries the same code as a daemon one — unless the
   command owns stdout (`search-like-rg`, hooks, the statusline) or already
   wrote part of an answer (`check-release --json`). For a success envelope it
   takes `result` and folds `epistemics`, `snapshot`, and `warnings` into it
   without clobbering same-named keys the op emitted.
5. `print_data` serializes the result. With `--json` it is compact on one
   line, otherwise pretty. A global 256 KB cap protects the agent's context
   window (`PIXEL_OUTPUT_CAP_BYTES=<bytes>` overrides it, `0` lifts it). A
   `--json` answer over the cap is cut structurally: the largest arrays are
   shortened, every other field survives, and the object gains
   `truncated: true`, `cap_bytes` and `truncated_arrays` (path, kept,
   total). Only when no array trimming can fit the cap does the output fall
   back to a `{truncated, cap_bytes, note, partial}` wrapper. Human notes
   such as graph-build announcements and lower-bound caveats go to stderr,
   never stdout.

So the CLI's `--json` output is the envelope's `result` with the honesty
fields merged in, not the raw envelope. Anything that needs the full
envelope talks to the daemon socket directly.

## Indexes and freshness

- The text index is git-anchored: base shards correspond to a commit, delta
  shards to changes since, and an overlay covers the dirty working tree.
  `pixel status` reports whether each layer is fresh.
- The graph is built lazily on the first graph command and updated per file
  by the daemon watcher. Without a daemon (CI, `PIXEL_DAEMON_AUTO_START=0`,
  a copied `.pixel/`), the first graph command after an edit compares the
  tree's per-file content hashes with the stored ones in one walk and
  re-extracts only the added/edited files, drops the removed ones and
  re-resolves the calls that targeted them (`pixel_graph::build::tree_delta`
  / `apply_tree_delta`). A full rebuild remains the fallback when the db has
  no freshness signature or when the drift exceeds
  `PIXEL_GRAPH_INCREMENTAL_MAX_PCT` percent of the indexed files (default
  `20`; `0` always rebuilds). The answer's `graph_build` says which path ran
  (`incremental`, `changed_files`, `removed_files`, or `reason`), and the
  stderr notice reads `updated graph.db for N changed file(s)`, `built
  graph.db on first use`, or `rebuilt graph.db: <reason>` (the file on disk
  is `graph.v2.db`). Call edges carry a resolution tier, and
  analyses report a lower bound when same-name call sites stay unresolved.
- Ruby constant receivers are resolved in `pixel-graph/src/resolve/ruby.rs`
  against class/module scopes, preserving the difference between nested
  declarations and `class A::B`. Unique class and constructor targets are
  exact; job and mailer conventions are probable. Incremental updates replay
  Ruby receiver edges because a new constant or factory override can change
  the target without redefining the called method. Stored receiver text
  normalizes AST-confirmed `Foo.new(args)` and `Job.set(args)` to `Foo.new`
  and `Job.set`; the written callee remains separate from the resolved target.
- Ruby methods generated by a literal declaration in a class or module body
  (`attr_reader`/`attr_writer`/`attr_accessor`, `alias_method`, `alias`,
  `delegate`, `scope`) are method symbols spanning that declaration
  (`pixel-graph/src/extract/ruby_generated.rs`); a `def` of the same method
  in the file replaces the generated one. An alias or delegator stores a
  `references` edge to its own owner's target (`arg_of` `:alias` /
  `:delegate`), never to the delegate's method, whose type is unknown.
- Ruby ancestors are stored as declared, in the graph's `ruby_mixins` table
  (owner, `superclass`/`include`/`prepend`/`extend`, `included:`-prefixed
  inside a concern's `included do`, the constant as written, its line;
  `pixel-graph/src/extract/ruby_mixins.rs`). `ruby::Index::lookup` walks
  Ruby's method lookup order from the caller's `self` — prepended modules,
  the owner, included modules and concerns, the superclass; on the class
  side the singleton methods, extended modules and each concern's
  `ClassMethods` (`class_methods do` defines it) — resolving each constant
  where the declaration is evaluated. A unique ancestor definition is
  `Probable`; a definition past an unnamed ancestor (dynamic `include`,
  external module), or several in an order reopenings in other files leave
  unproven, is unresolved; the owner's own definition keeps the existing
  own-method rules. An incremental batch that changes a declaration, or a
  constant a stored declaration ends with, sets `Affected::ruby_ancestors`
  and replays every Ruby `self` call and method-symbol reference.
- Ruby files are known by extension, by name (`RUBY_FILE_NAMES`: `Gemfile`,
  `Rakefile`, `Guardfile`, `Capfile`), or, for an extensionless file in a
  `bin/` or `exe/` directory, by a Ruby shebang (`lang_of_file`); the graph
  walks read such binstubs and extraction drops the ones that are not Ruby.
  Ruby requires resolve in `pixel-graph/src/imports/ruby.rs`: a directory
  with a `Gemfile` or `*.gemspec` is a project, a file belongs to the
  nearest one, and `require` searches only that project's `require_paths`
  (default `lib`) and its Gemfile's local path gems; a path whose prefix
  names an external gem of the project (Gemfile, or `Gemfile.lock` specs,
  transitive included) never resolves locally, and a miss never escapes to
  a sibling project. `require_relative` is relative to the requiring file.
  The manifests are read as literals from disk at each build and update; an
  update touching a Ruby file or manifest re-resolves every Ruby import. A
  `Gemfile.lock` is never stored as a graph file, but the graph walks hash
  it into the freshness signature (`is_graph_candidate`), so an edit to it
  alone makes the graph stale and the delta that applies it re-resolves
  every Ruby import. Every walked file extraction keeps no `files` row for
  (a lockfile, a binstub that is not Ruby, a generated blob) has its content
  hash in the graph's `walked_files` table, which `rows_match_tree` and
  `tree_delta` read beside the source rows: an unchanged one neither
  withholds an update's signature nor reappears in every delta.
- Rails routes (`config/routes.rb` and drawn `config/routes/*.rb`) are read
  statically by `pixel-graph/src/extract/ruby_routes.rs`: verbs, `root`,
  `resources`/`resource` (`only`/`except`/`controller`/`path`/`param`),
  `member`/`collection`, `namespace`, `scope`, `controller` blocks and
  `mount`. Each route is a `route` concept (`raw` `POST /admin/orders`,
  `detail` the handler, which `find-code` matches return as `detail`) and a
  `references` edge from the routes file to the controller action
  (`arg_of` `:route Admin::OrdersController`), resolved to that class's own
  or inherited action only. Active Record associations reference their
  model class (`arg_of` `:association has_many LineItem`), looked up in the
  owner's namespaces as `compute_type` does; `through:` without
  `class_name:`, `polymorphic:` and computed names reference nothing.
- The `graph` op rebuilds from scratch by default (`rebuild-graph`,
  `prepare-repo --rebuild-graph`). With `"if_stale": true` (a request field
  that defaults to `false` and is sent only when set, so an older daemon
  ignores it and rebuilds), it takes the same keep / update / rebuild
  decision as the first graph command above; `prepare-repo` sends it. A
  `graph` answer from a daemon that knows the field carries `build`
  (an older one sends none, and `prepare-repo --json` then shows
  `timings.graph.build: null`): `{"mode": "fresh"}`, `{"mode":
  "incremental", "changed_files", "removed_files"}` or `{"mode": "full",
  "reason"}` (`requested` for an explicit rebuild), and `phases`: the full
  build's phase timings plus `publish_ms` (and `check_ms` when `if_stale`
  chose the rebuild), or `check_ms` (the walk that chose) and `apply_ms` for
  a kept or updated graph. `prepare-repo --json`
  moves both into `timings.graph`, beside `timings.index` (how each index
  layer was obtained) and `timings.total_ms`.
- History facts are ingested by a dedicated low-priority thread. Queries
  never wait on ingest; they answer from what is already in `history.db` and
  carry its `index_state` (`diffs_evicted`, `diff_coverage_since`). Diff text
  is bounded by an age window (`PIXEL_HISTORY_WINDOW_DAYS`, default 365) and
  a size budget (`PIXEL_HISTORY_BUDGET_MB`); commit metadata is never
  evicted. Both compare `unixepoch(committed_at)`, the author date, so a
  date whose offset SQLite cannot read (git prints `+518:00` for an object
  holding `+51800`) is stored as the same instant in UTC
  (`normalize_committed_at`). So a `file-history --file` answer is complete once phase A is,
  while a `file-history --token` answer only sees indexed diffs and says
  what it missed in `coverage` (`pixel_facts::lifecycle::DiffCoverage`):
  `lower_bound` when any commit's diff is pending or evicted (more touches
  may exist), `first_seen_exact` when none of them was authored at or
  before `first_seen`, and a `note` with the `git log --reverse -S` command
  that checks the full history. A token found nowhere carries the same
  block.

## Agent integration

The provider-neutral task layer lives above retrieval. `run-hook task-event`
normalizes Claude, Codex and Pi events into `task_bridge`; it never changes
the retrieval daemon's wire protocol. Global Claude/Codex installation owns
one native lifecycle registration; Pi's project extension owns its lifecycle.
A project-only native install still relies on the global lifecycle layer.
Foreign hooks and trust settings are preserved. Doctor distinguishes registered
hooks from actual observations; neither establishes complete host coverage.

`pixel-task::policy` decides edit/completion eligibility from the contract and
current source. CLI `task_prepare` bundles scope, bounded impact and test
suggestions. Unknown graph coverage requires explicitly configured conservative
checks. Scope is advisory, and static lower bounds never establish absence of
consumers. Source changes invalidate preparation/review and make check receipts
stale. Checks run in private materialized source workspaces. Unsupported source
dependencies remain blocked rather than silently omitted.

The bridge binds by worktree, provider and session, with Pi branch bindings
carrying their task and attempt across forks. Automatic first-mutation fallback
creates evidence state but denies that triggering edit. Missing state denies
supported edits and verified completion while read-only and task-recovery
commands remain available. A missing completion requirement can trigger at most
three automatic corrections, and at most two for identical unresolved state;
cancellation ends correction. These are host-hook guarantees, not a sandbox for
arbitrary shell programs. A host stopping does not imply verified completion.

Routing uses only deterministic eligible actions. Optional warm local
classification ranks those actions with a 300 ms budget and caches one result
per decision input/policy/configuration. Classifier failure falls back to the
original order. Predictions cannot satisfy checks. See
[task contracts and evaluation](docs/task-optimizer.md) for configuration,
commands, migration and operational limits.

Task trajectory events live in the durable task journal. `actions.jsonl` remains
best-effort invocation accounting, with optional explicit task correlation.
Blocked/retried model requests count as work; coordinator and classifier calls
are separate. Missing host/child coverage produces a lower bound, never zero
work. `task replay` reads frozen policy frames without execution. Empirical regret
compares only matching, successful, completely observed attempts. The existing
`eval/score.py` and `eval/gate.py` own quality/turn regression gates; the controlled
container backend adds isolated three-arm trials, not a replacement arena.

A global `pixel install` deploys no agent prompt and wires no retrieval
guard: every agent keeps its native search. It removes the
`~/.local/share/pixel/agent-prompt.md` and `subagent-prompt.md` copies earlier
releases deployed, then handles each agent through its own extension point:

- **Claude Code**: task lifecycle hooks in `~/.claude/settings.json`. Retired
  retrieval prompt, post-edit and metrics registrations are removed; task
  contracts and foreign hooks are preserved. The optional plugin ships no
  skill and registers no hooks; the per-prompt brief comes from the task hook.
- **Codex**: task lifecycle hooks in `$CODEX_HOME/hooks.json` (default
  `~/.codex/hooks.json`). Retired permanent `developer_instructions` blocks,
  retrieval prompt and metrics registrations are removed. The optional
  plugin ships no skill and registers no hooks.
- **Pi**: when Pi's agent directory (`$PI_CODING_AGENT_DIR`, else
  `~/.pi/agent`) exists or `pi` is on `PATH`, a local Pi package under
  `~/.local/share/pixel/pi-package/` (`package.json` and
  `extensions/pixel-impact.ts`, which registers the explicit
  `/pixel-impact <symbol>` command) and its absolute path in the `packages`
  list of Pi's `settings.json`, which keeps every other key and package. A
  managed copy an earlier release wrote to `extensions/pixel-impact.ts` is
  removed so Pi does not register the command twice; a foreign file there is
  left and reported yellow. A settings file that does not parse, or an agent
  path that is not a directory, is left untouched and reported yellow.
  Pixel's retired `APPEND_SYSTEM.md` block is removed, foreign text kept.
  Accepting a classify engine additionally offers the `pixel-classify` skill
  (`skills/pixel-classify/SKILL.md` under the agent's config dir) and the
  `pixel-classify-files` tools, which judge a file's contents without loading
  them into context, as a second Pixel-owned package
  (`~/.local/share/pixel/pi-classify/`, declared in the same `packages` list).
  Uninstall removes both packages and their entries.
- **OpenCode, Devin, Antigravity, zcode, Cursor, Copilot CLI**: nothing is
  written. Install removes what earlier releases wrote when it is still there:
  the OpenCode `AGENTS.md` block and `plugins/pixel.js` guard (in
  `~/.config/opencode`, `$XDG_CONFIG_HOME` honoured), Devin's hooks in
  `~/.config/devin/config.json`, the Antigravity plugin, its config entry and
  the global `pixel-guard` in `~/.gemini/config/hooks.json` (a user-defined
  hook under that name is kept), the zcode guard in
  `~/.zcode/cli/config.json`, `~/.cursor/hooks.json` and
  `~/.copilot/hooks/pixel.json`. `doctor` reports a leftover as red.

The `policy` key of `pixel config` (`advisory`, `enforce`, `off`) is retired:
no hook reads it and every value leaves native tools untouched. It stays
readable and writable so an existing configuration file keeps validating.
Install and uninstall rewrite only recognized Pixel managed content and
registrations, preserving foreign settings and hooks.

The focused capability calls `pixel impact SYMBOL --no-refresh`. This CLI path
bypasses the daemon and all index maintenance: a read-only SQLite transaction
checks graph schema/extractor metadata and the current source signature, then
answers through the daemon's own impact entry point
(`pixel_daemon::api::impact_on_graph`): the same name resolution, the same
`{candidates, hint}` reply on an ambiguous name, and the same default depth
(`IMPACT_DEFAULT_DEPTH`, 3) and per-depth item limit. The signature check
(`pixel_graph::build::freshness_signature_trusting_stat`) reads and hashes
only files whose mtime is not older than the last full build's start
(`BUILD_STARTED_KEY` in the graph's `meta`, less a 2 s clock margin) or that
the graph does not hold; older files reuse their stored `blob_oid`. That is
the stat trust the daemon's in-process `TreeHashCache` makes, here across
processes: an edit that also restores an mtime older than the build is not
seen. A graph built before the key existed gets the full content signature.
Repository discovery, freshness checks and query work share a 1,500 ms
deadline. Depth is limited to 1–3 and serialized results to 32 KiB. Missing,
stale, incompatible or slow input fails back to native retrieval. Workspace
fan-out is incompatible with this mode.
Read-only SQLite access may create its WAL coordination sidecars, and normal
CLI invocation accounting still applies; this mode does not promise zero
filesystem writes. It never creates, migrates or refreshes graph data.
Results retain graph truncation markers and an open-world epistemic envelope;
no callers found does not establish that none exist.

`pixel install --repo <path>` migrates the project integrations to the native
default and adds no guard; each file that names this machine's binary is
listed in the clone's `info/exclude` so no machine path is committed:

| File | Agent | Content |
| --- | --- | --- |
| `.claude/settings.local.json` | Claude Code | Removes the retired retrieval guard and restores adopted RTK registrations; preserves foreign hooks and independent task controls; neither created nor rewritten when there is nothing to remove |
| `.codex/config.toml`, `.codex/hooks.json` (legacy backup sidecar during migration) | Codex | Removes retired Pixel `developer_instructions` text and restores foreign hooks from the owned composed-guard backup; removes duplicate task registrations only when enabled global hooks cover every event and their current definitions are approved; preserves project-only task controls and skips tracked files or paths aliasing the global hook file; `.codex/hooks.json` is neither created nor rewritten when there is nothing to remove |
| retired managed block in `AGENTS.md` | any agent that reads `AGENTS.md` | Removed on install so ordinary tasks carry no permanent Pixel retrieval instructions; foreign project instructions are preserved |

The same run removes two retired project files: Pixel's hooks in
`.devin/config.local.json` (other entries kept) and the Pi project extension
`.pi/extensions/pixel-guard.ts` (with the older `.pi/agent/` copy).
`repo.devin-hooks` and `repo.pi-guard` report either one still there.

`doctor` checks them under the `repo.*` ids. `install --repo` and `uninstall
--repo` also remove the MCP server entry releases up to 0.6.1 wrote into
`.warp/.mcp.json`, and `repo.warp-mcp` reports one still there.

Automatic Codex caller-facts injection is retired.
Global and project installation remove the retired `<!-- pixel:managed:begin
-->`/`end` block from `developer_instructions` with `toml_edit`, preserving foreign
text, unrelated configuration and layout. A key with no remaining instructions
is removed. Invalid TOML is refused rather than replaced. `doctor` checks that
the retired block is absent. `$CODEX_HOME` remains honoured; no shell wrapper or
replacement system prompt is installed. Pi's shared
`~/.pi/agent/APPEND_SYSTEM.md` keeps foreign instructions; installation and
uninstallation remove only Pixel's recognized block or historical prompt.
OpenCode's cleanup strips the managed block from its global `AGENTS.md`
(deleting the file when it held nothing else), removes the `plugins/pixel.js`
guard Pixel wrote (a plugin of that name it did not write is kept) and drops
`opencode.json` `instructions` entries naming the retired prompt. A config that
does not parse as strict JSON is skipped, not rewritten.
`install.opencode-agents-md` is red while the block or the managed plugin
remains, and passes when OpenCode is absent.

`pixel run-hook` (alias `hook`) has one live entry point, `task-event`, and it
is the only hook `pixel install` registers (Claude Code and Codex):

| Hook event | Command | Effect |
| --- | --- | --- |
| `SessionStart`, `UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `Stop`, `SessionEnd`, `SubagentStart`, `SubagentStop`, plus `PostToolUseFailure` (Claude) or `Interrupt` (Codex) | `pixel run-hook task-event --provider <host> --event <event>` | Binds coding objectives, gates edits, records tool outcomes, and bounds Stop correction. Global native hooks compose with existing hooks. Once enforced, a task retains its gates if runtime settings change. On `prompt-submit` it also returns the `[PIXEL:BRIEF]` evidence brief as `additionalContext` (see below). |

The verbs earlier releases registered (`guard`, `composed-guard`,
`session-start`, `prompt-submit`, `post-compaction`, `post-tool-use`,
`metrics`) are still accepted and exit 0 with no output, so a registration an
old install left in an agent's settings cannot block or fail a host. `pixel
install` and `pixel uninstall` remove those registrations from every agent
file above; `crates/pixel-install/src/routing.rs` recognises them by verb.

The brief (`execution_brief/chain.rs`, `execution_brief/evidence.rs`) is built
on every Claude Code and Codex `prompt-submit` task event for a code-shaped
prompt. Pi gets none (`start_brief` in
`task_hook.rs` skips `TaskProvider::Pi`), and no other host registers a prompt
hook. It runs at most four ops under one 750 ms deadline: `search-content -F
-l` on the first anchor, `find-symbol` to resolve a uid, `impact <uid>` only
for change or caller intent, and `find-code` when no anchor found a file. It
reads a warm daemon if one answers this protocol and otherwise the index and
graph read-only in process; it never starts a daemon and never builds an
index or graph. A repository without an index yields no brief. A missing or
stale graph still yields the text evidence (`files:`), with the reason under
`unresolved:` (`the graph is not built`, `the graph is stale`) and no callers.
Output is capped at 2 KiB and says how many ops ran (`ops: n/4`,
`partial: budget` when cut short). `PIXEL_BRIEF=0|false|off` or `brief: false`
in `.pixel/config.yaml` switches it off.

`pixel doctor` checks current installation artifacts and distinguishes configured
or protocol-checked hooks from observed live execution.
Every check is listed in `pixel_install::doctor::CHECKS` with a stable id and
the command that repairs it (`pixel doctor --list`): `--only`/`--skip` select
by id or by group (`install.*`), and each yellow or red check reports that command as `fix`.
`install.claude-hooks` is also yellow when complete hooks run another binary than
the managed one doctoring them (a global `pixel-dev install`, a previous release's
path); a `pixel-dev` doctor never judges the home install.
`repo.claude-hooks` checks that retired Pixel retrieval callbacks are absent;
foreign shell hooks no longer need to make room for a Pixel rewriter.
`--fix` runs them: `repair_plan` folds the flagged checks into one run of each
distinct catalogue command, in catalogue order, `run_repair` executes it with
the running binary, and the checks are re-run so each repair is judged
`fixed`, `not_converged` or `failed` from the new report, not from its exit
code. A command only one outcome names (the `rm` of an orphaned RTK backup)
is never run. The binary checks go beyond the artifact: `binary.shell-path`
asks the resolved login shell itself (or `--shell`) for `pixel`, because an
agent harness that inherits an environment where `pixel` is not a command
silently works without it. The exit code carries the verdict: 0 when no check reaches `--fail-on` (default `red`),
1 when one does, 2 when the checks could not run.

### Invocation accounting and chat delivery

`pixel-actionlog` owns local metrics, not a parallel observability engine. A
top-level invocation correlates outcome, measured elapsed duration and rendered
output bytes with a versioned native-workflow estimate. Existing logs remain
readable. No additional retrieval, native comparison command, model request, or
repository sweep is justified solely by metrics calculation.

The byte approximation is roughly one token per four UTF-8 bytes and includes
reporting overhead. Measured output covers rendered CLI stdout, CLI-owned diagnostics and top-level
errors, not lower-level library or subprocess streams. V1's fallback volume policies are 4 KiB per assumed distinct
returned file read and 1 KiB per native-command output. They are assumptions, not
measured averages. `workflow-v2` measures the one case it can: `list-signatures`
stands in for reading one whole file, so its baseline is that file's size (a
`stat`, no source read) with no assumed command, and the live line states
`full read N tok, pixel answer M tok (-X%)` from the file and the stdout answer
(`answer_bytes`), both floored bytes / 4 as in `scripts/bench-read-savings.sh`.
Records keep the version they were written with. Estimates consider only returned evidence/relationships and
represented native steps. Partial results remain partial; meaningless comparisons
are unavailable; zero and negative savings are retained. There is no external
telemetry, hidden reasoning estimate, or monetary claim.

Time accounting is separate from byte accounting. An optional `time_estimate`
records `estimator_version: sequential-v1`, `round_trip_ms`, `native_command_ms: 0`,
`sequential_steps`, and signed `saved_ms`. Legacy records lacking these fields
remain unavailable for time comparisons; they are not silently recalculated.
`pixel token-savings` adds `time_estimates` groups keyed by token/time estimator
versions, effective assumptions and coverage, preserving earlier summaries.

Time savings are a separate `sequential-v1` workflow estimate, not measured
LLM latency. Let `steps = native_commands + distinct_files`; relationships do
not add round trips. A zero-step baseline is unavailable; otherwise the estimate
in milliseconds is:

```text
max(steps - 1, 0) * round_trip_ms - measured_pixel_duration_ms
```

One shared initial LLM/tool round trip cancels. The default policy assumes
**2000 ms per sequential round trip** and **0 ms of native command execution**.
`PIXEL_METRICS_ROUND_TRIP_MS` overrides the round-trip assumption with an unsigned
integer number of milliseconds (zero is allowed); unset, invalid, non-UTF-8, or
overflowing values use 2000. These assumptions and the estimator version are
recorded with each new invocation, not applied retroactively to old records.
Batching or parallel native workflows may require fewer round trips: this is
not a measured end-to-end speedup or a guarantee. Negative time savings are
retained; missing evidence is unavailable, and capped comparisons are partial.

The authoritative `🟩 Pixel · ...` line distinguishes `tokens saved (workflow
estimate)` from seconds `saved (sequential estimate)`. Both labels mark partial
comparisons. Measured execution duration stays distinct from both estimates;
no extra model call or native benchmark is run to compute the time estimate.
A missing comparison is never a silently dropped row: it renders
`unavailable: <reason>` (no policy baseline, failed operation, a render-cap or
depth-cap refusal, an uninitialized accumulator, or a zero-step baseline), and
a baseline that saves nothing renders `no estimated … saving`. The reason is
recorded as `comparison_gap` — a zero-step baseline is the one inferred from
the recorded evidence at render time — so a replay of the record states the
same cause.

Ordinary CLI boundaries emit an authoritative metrics line on stderr after the
result/error without changing JSON stdout. On a failure the `pixel: <error>`
diagnostic is repeated after that line, so the last stderr line still names the
failure when a caller reads only the tail. `--metrics=off` and `PIXEL_METRICS=0`
disable live reporting (and the repeat), not local accounting. Metrics failures cannot change success or safety behavior.
Exact-output search compatibility, hooks, protocol streams and statuslines remain
untouched; a separate host-supported channel is required for their live relay.
Protected paths lacking output-volume capture retain unavailable volumes rather
than fabricate counts.

The active prompt instructs an agent to copy the exact line from the same tool-call
result once, skipping an invocation already relayed by the host. A global latest
record is unsafe under concurrency and must never be used. The installer provides
no native automatic chat transport; mock-wrapper tests prove prompt delivery and
stream/exit preservation, not actual model adherence or live-host duplicate
suppression. Chat relay remains a host-supported, separately verifiable boundary.

## Testing and gates

- The public `.github/actions/setup-pixel` composite action installs a checksum-verified
  release into runner temporary storage and optionally prepares indexes without a
  daemon. `setup-pixel.yml` smoke-tests real releases on Linux x64/ARM64 and
  macOS ARM64; `scripts/test-setup-pixel.py` checks installer failure contracts.
- After publication, `release.yml`'s `virustotal` job submits the three
  release archives to VirusTotal and appends a link to each report (by
  sha256) to the release notes; without the `VT_API_KEY` secret it only emits
  a notice.

- Unit tests live next to the code in each crate. `pixel-daemon` tests build
  small git fixtures in a temp dir and call `Service::handle` directly.
- CLI integration tests in `crates/pixel/tests/cli/` (one binary, one module per file) invoke the built binary
  through `CARGO_BIN_EXE_pixel` against a temp fixture repo.
- CI (`.github/workflows/ci.yml`) classifies the diff first, so a job whose
  paths did not change skips its steps, then runs in parallel:
  - **Test + Format**: action-pin verification (`scripts/verify-action-pins.py`),
    the SPDX header check (`scripts/check-spdx.py`, on every diff),
    `cargo fmt --check`, `cargo nextest run --profile ci`
    (`.config/nextest.toml`: one process per test, retry once but fail on
    flaky, kill after 180 s), `cargo test --doc`, a check that the tests left
    the checkout's `.pixel/actions.jsonl` alone, the `scripts/test-*.py`
    contract scripts (installer, gate runner, pre-push no-op,
    release prepare, Homebrew formula and Linux
    bottles, release SBOM, homebrew-core formula,
    nightly diff checkpoints, coverage selection, mutants
    config, action pins, advisory ignores, SPDX headers, clean, cancel-stale sweep, harness-grid dispatch input,
    reproducible release build environment, the `eval/` agent A/B harness against fixture CLIs), the
    pixel-retro lead-time and adherence contracts
    (`.agents/skills/pixel-retro/test_lead_time.py`, `test_adherence.py`)
    and the Bun Pi impact-command contract (`scripts/test-pi-impact.mjs`);
  - **Lint**: `cargo clippy --all-targets` with warnings denied, then
    `cargo check` of the two reduced feature lanes (`--no-default-features`,
    `model2vec` only);
  - **MSRV**, **Ranking gates** (the NDCG@10 bench in test mode) and
    **Dependency policy** (`cargo deny`, then
    `scripts/check-advisory-ignores.py`: `osv-scanner.toml`, which Scorecard
    reads, accepts the same advisories as `deny.toml`).
- Other workflows: `mutants.yml` (01:17 UTC on `main`, only the cumulative
  diff since the latest completed campaign; no pull-request trigger).
  `scripts/mutants-nightly-range.py` selects a checkpoint from trusted
  completed main-run artifact metadata and refuses API/history failures.
  A checkpoint is uploaded after every listed mutant has a recognized
  outcome; survivors keep the run red, while missing outcomes and disk-full
  failures leave the prior checkpoint. Unchanged main starts no Rust jobs.
  The existing plan/shards/report share `.cargo/mutants.toml` and distribute
  shards over `PIXEL_MUTANTS_SHARD_RUNNERS` (GitHub-hosted `ubuntu-26.04`
  by default). Only scheduled CI executes mutations; there is no manual,
  local or pull-request campaign. The pre-push hook is a no-op; local validation is optional.
  Other lanes include
  `cross-build.yml` (the three release lanes),
  `reproducible-build.yml` (the `x86_64-unknown-linux-musl` release binary
  built twice from two checkouts at different paths, no cache, failing
  unless the two sha256 match; on pull requests touching the build
  environment, `Cargo.lock`, a `Cargo.toml`, `crates/pixel/build.rs` or
  `release-build.yml`, on pushes to `main` touching any of them or
  `crates/**`, weekly and on demand), `release.yml`
  (on a tag: `verify`, then publication, the tap and the post-publish smoke
  test) and `release-build.yml`, the reusable workflow it calls to build the
  archives, write the formula and the Linux bottles with
  `scripts/homebrew-formula.py` and the homebrew-core formula with
  `scripts/homebrew-core-formula.py`, write each archive's CycloneDX SBOM
  `pixel-<tag>-<target>.cdx.json` (cargo-cyclonedx, narrowed by
  `scripts/release-sbom.py` to the crates `cargo tree -p pixel-cli` compiles
  for that target and feature set, from the build matrix's `features`), and
  sign their provenance, the SBOMs among the subjects (it is the
  attestation's signer, which makes the provenance SLSA Build Level 3; the
  signed Sigstore bundle ships as the release asset `pixel-<tag>.intoto.jsonl`,
  the suffix Scorecard's Signed-Releases check reads as provenance, and the
  smoke test verifies it, the archive and its SBOM with `gh attestation
  verify`, with and without `--bundle`), `homebrew-core.yml` (that formula
  built from source, `brew test`, `brew audit --strict --new`, on macOS and
  Linux),
  `release-prepare-scope.yml`, `pages.yml` (the website) and `scorecard.yml`
  (OpenSSF Scorecard on every push to `main` and weekly: publishes the score
  to `api.scorecard.dev` and the findings to code scanning) and `codeql.yml`
  (CodeQL on every pull request into `main`, every push to `main` and
  nightly at 05:41 UTC: workflows, Python and JavaScript/TypeScript always
  scan; Rust runs only after merge, nightly or via manual dispatch. To scan
  a sensitive branch before merge, dispatch the workflow on that branch;
  inspect its run and code-scanning results before merging. Rust findings
  are post-merge feedback and must be triaged before the next release.
  Scans use `build-mode: none` and default security queries, preserving
  eligibility for incremental analysis in supported languages. The existing
  CodeQL merge-protection rule remains enabled for PR analyses. The three
  `Analyze (actions)`, `Analyze (python)` and `Analyze (javascript-typescript)`
  jobs are required checks from GitHub Actions in the main ruleset. GitHub's
  aggregate CodeQL check can be neutral because main has a Rust configuration
  absent on PRs; the required jobs still enforce completion of the PR scans.
  A dedicated Rust extraction cache is keyed by runner, compiler,
  manifests/lockfiles and workflow. Manual branch scans may restore it;
  only successful main analyses save it) and
  `fuzz.yml` (`cargo deny` on the `fuzz/` workspace with the root
  `deny.toml`, then every cargo-fuzz target on nightly: 60 s each on a pull
  request touching `fuzz/`, `pixel-graph`, `pixel-index`, `pixel-git`, the
  root `Cargo.toml` or `deny.toml`, 600 s weekly and on demand, 120 s when
  `release.yml` calls it on a `v*` tag, crash reproducers uploaded) and `coverage.yml` (the Test job's nextest suite
  under `cargo llvm-cov`, doctests aside, at 02:47 UTC on main only.
  `scripts/coverage-nightly.py` skips both expensive jobs when the latest
  successful scheduled run already measured this SHA; failed or cancelled
  measurements are retried. Read-only Actions access supplies run metadata.
  Reports contain line, region and function totals and one row per crate in the job
  summary, the report as the `coverage-summary` artifact; it fails on a red
  test or on line coverage under 90%, the OpenSSF gold bar; its `branches`
  job runs the same suite on a dated nightly under `cargo llvm-cov
  --branch` and writes branch and line totals, one row per crate, to its
  summary; it uploads the raw JSON report as the `coverage-branch-summary`
  artifact and the per-line lcov report as `coverage-branch-lcov`, which
  `scripts/coverage-uncovered-branches.py` turns into uncovered branches per
  file and line).
- `fuzz/` is a cargo-fuzz crate with its own `[workspace]`, outside the
  root workspace (no root `cargo` command builds it). `graph_extract` feeds
  arbitrary source to `pixel_graph::extract::extract_file` (no panic, lines
  inside the file, `enclosing_index` inside `symbols`); `search_plan` checks
  that `pixel_index::plan::plan_pattern` never drops a document the
  verifier's `grep_regex` matcher matches, for both gram extractors.
- Local compilation, tests, lint, review and installation are optional
  diagnostic tools. Publish the candidate promptly; CI must validate its
  current head before merge (CONTRIBUTING.md, "Agent validation workflow").
  A chosen background local check uses an unchanged snapshot and records its
  SHA. Rebuild, reinstall, index and doctor are only needed for a chosen
  installed-path diagnosis or explicitly requested local deployment.

## Release gate

`pixel check-release <version|tag> [--repo <path>] [--json]`
(the `pixel-release` crate) runs in the first job of
`.github/workflows/release.yml` (`verify`, before `cargo deny check` and its
`cargo test`) and is a
maintainer's last local step: it
reads `Cargo.toml`, every member's manifest, `Cargo.lock` and
`CHANGELOG.md` and reports three checks (`cli-version`, `cargo-lock`,
`changelog`), exit 1 on any failure. Pure functions over file contents;
no git, no network.

After `verify`, the `fuzz` job calls `fuzz.yml` (every cargo-fuzz target,
120 s each) on the tagged commit. The `assets` build needs both `verify`
and `fuzz`, so no release is built from code that was not fuzzed.

The release skill also runs `.agents/skills/release/check-candidate.py` on
recorded base and prepare SHAs. Before merge it requires the fetched target
to equal that base and a prepare-only diff without remaining fragments.
Before tagging it requires the squash merge on the target history, its sole
parent equal to that base, and its tree identical to the validated prepare
head. Later target commits do not change which merge gets tagged. Its real
Git contract runs in both `scripts/gates.sh` and CI
(`scripts/test-release-candidate.py`). CI results and semantic changelog
coverage remain separate release requirements. The explicit `--maintenance`
mode permits backport code in the diff (full CI is required), retaining the
ancestry, empty-fragment and exact-tree checks.

## Build provenance

`crates/pixel/build.rs` captures the commit (`-dirty` when tracked files
were modified), target triple, rustc version and build date
(`SOURCE_DATE_EPOCH` honoured) at compile time and `pixel --version` prints
them under the version line (`pixel -V` stays one line). Every value falls
back to `unknown` rather than failing the build. The script re-runs when
`.git/HEAD`, the ref it points to (looked up in the worktree's git dir and
the common dir), the index, or `SOURCE_DATE_EPOCH` changes, so the flag
follows commits without a `cargo clean`.

Release builds are reproducible. `scripts/release-build-env.sh` sets
`SOURCE_DATE_EPOCH` to the commit's time and `RUSTFLAGS` to
`--remap-path-prefix` the checkout (`/pixel`) and `CARGO_HOME` (`/cargo`);
`release-build.yml` and `cross-build.yml` run it before their cache step
(rust-cache hashes `RUSTFLAGS` into the key they share), `Cross.toml`
forwards `SOURCE_DATE_EPOCH` into cross's container, and the three
workflows pin the same `cross`. `reproducible-build.yml` checks it
(`## Testing and gates`); SECURITY.md, "Reproducing a release build", is the
user's procedure.

## Build features

`pixel-cli` defaults to `fastembed` and `model2vec`. `fastembed` needs ONNX
Runtime and cannot build for musl, so Linux release binaries are built with
`--no-default-features --features model2vec`. `--no-default-features` alone
gives an offline-only binary with no semantic search.

One build-time switch sits beside the features: `PIXEL_UPDATE_CHECK=off`
(read with `option_env!` in `update_notice.rs`) builds a binary that never
checks for a release, prints the notice or offers to upgrade itself. The
homebrew-core formula (`scripts/homebrew-core-formula.py`) builds with it and
with `--no-default-features --features model2vec`, since homebrew-core builds
from source, owns updates, and refuses the prebuilt ONNX Runtime `fastembed`
downloads; every other build keeps the check.
