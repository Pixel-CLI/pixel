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
   │  agent prompt + hooks / extensions     (pixel-setup)
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
MCP server. `pixel setup` registers no MCP server with any agent.

## Crates

| Crate | Role | Depends on (pixel crates) |
| --- | --- | --- |
| `pixel-cli` (bin `pixel`, in `crates/pixel`) | Command-line surface. Parses argv with clap, talks to the daemon or runs the service in-process, prints text or JSON. Also hosts the hook entry point under `run-hook` (`task-event`, `metrics`). | every library crate except `pixel-context` (reached through the daemon), `pixel-ultraflow`, `pixel-session` (reached through the daemon and the ranker), `pixel-bench` and the imported `mvp-store` and `mvp-index` (of the imported crates it reads mvp-model only, for the brief's French prompts: see the dependency rule below) |
| `pixel-proto` | The shared contract crate: `Envelope`, `PixelError` and `ErrorCode`, `Epistemics`, `SnapshotInfo`, `Budget`, `Warning`, and the `Op` request enum. No I/O, no business logic. Every other crate that speaks the wire format depends on it, and it depends on nothing internal. | none |
| `pixel-daemon` | Transport-agnostic `Service` (`api.rs`) and the Unix-socket NDJSON daemon with filesystem watching (`daemon.rs`). Dispatches each `Op` to the right library, attaches snapshot and epistemics metadata, and is the one place a retrieval envelope is built. Also hosts the recall daemon service. | index, graph, context, rank, proto, ops, facts, session, recall, git |
| `pixel-index` | Sparse n-gram (trigram) text index: gram extraction, window weighting, posting-list algebra, git-anchored base and delta shards, working-tree overlay, query planner, verification, and the `gitsync` helpers that read HEAD, branch, and porcelain status. | git |
| `pixel-graph` | Code graph: tree-sitter extraction of symbols, imports, and call sites per file; import resolution; tiered call resolution with an epistemic envelope; and the analyses `impact`, `trace`, `process`, `cluster`, `changes`, `targets`. `store` owns the SQLite schema. `mvp_adapter` reads the imported pixel-mvp index (`refresh` brings it up to date under a state directory, through git) and returns what its rules decide at each Rust site in this graph's rows: proven calls as `exact` edges, references as `probable`, the rest as `unresolved_calls` rows, and each Rust import's resolved file, on this graph's own symbols (rows with no symbol here, Rust test code, are dropped and counted); the Rust source switch, on by default for a Cargo crate, writes them in ("Indexes and freshness"). `mvp_adapter::ruby` pairs pixel-mvp's Ruby sites with this graph's (kind, file, line and the name written; then, within one enclosing symbol, in line order) and plans the hybrid of ADR-0037 there: pixel-mvp's targets where it resolves a site, this graph's own edge where it abstains or has no such site. `shadow` builds this graph and the adapter's rows in a scratch directory and counts what each links alone (`pixel graph-shadow`); `shadow::ruby` counts the Ruby merge instead: the edges both hold, this graph's kept and replaced, pixel-mvp's added, for calls, routes, associations and other references (`pixel graph-shadow --lang ruby`). | git, index, mvp-index, mvp-model, mvp-store |
| `pixel-facts` | History-wide fact and diff ingest, search, lifecycle, and rescue discovery. Owns `history.db`: commit metadata, diff text, and contentless FTS5 trigram indexes over the diffs and paths. Demand-driven: the daemon never ingests at startup, and `status` and `doctor` never create the db — the first history query runs a bounded catch-up (`PIXEL_FACTS_QUERY_BUDGET_MS`, default 3 s) and spawns the low-priority keep-fresh loop that never blocks queries. The daemon `history`, `dig-history` and `lifecycle` ops take `read_only: true` (protocol 15), under which an absent db answers `unavailable` and a stale one answers from existing rows — ingest and catch-up are skipped; the prompt hook sends only this form. Bounded: diff text is kept for the last 365 days (`PIXEL_HISTORY_WINDOW_DAYS`, `0` for all) within 256 MiB of used pages (`PIXEL_HISTORY_BUDGET_MB`), newest first; older diffs are evicted, metadata never is. `build-index --history` remains the explicit full build. Backs `excavate`, `lifecycle`, `history-search` and `rescue` discovery (`resolve` is the graph's concept index). | git |
| `pixel-rank` | Fusion core for `targets` and ranked `search`: task text and signal inputs in, closed prioritized P0/P1/P2 file list out. The scoring is pure; `compute_signals` gathers the activity channel itself (git log churn when facts have none, and a failed or capped scan is reported as unavailable rather than as an empty map) and takes the session and error-sink channels from its caller — the daemon feeds neither of those. Also hosts `regions`: the pure symbol-level regions analysis for `scope-task --regions` (P0 symbols + call/import edges in, regions manifest out — line ranges, conservative conflict pairs, merge-order layers, shared files). | graph, git, session |
| `pixel-context` | Semantic compression of code-context items: layered renderings that fit a token budget instead of raw source dumps. | none |
| `pixel-ops` | Safe git mutation infrastructure ported from usable-git: snapshot store, repository lock, operation journal, recovery keys. Implements `inspect`, `review`, `history`, `diff`, `publish`, `push`, `ship`, `branch`, `update`, `sync`, `reconcile`, `rewrite`, `branches`. | git |
| `pixel-git` | The single git subprocess wrapper for the workspace. Replaced three earlier ad-hoc wrappers. Any crate that shells out to git goes through `GitRunner` (timeout, output cap, redacted stderr). The runner waits on completion notifications from its pipe workers and an exit observer (`waitid` with `WNOWAIT` on Linux/macOS), keeping one deadline across child exit and every pipe; `crates/pixel-git/tests/boundary.rs` fails the build on a `Command::new("git")` in any other crate's non-test code. Also owns the trust boundary of `.pixel/`: `sidecar` refuses a `.pixel` that is a link or that git tracks and creates owner-only directories without following a link, `nofollow` opens files without following a link at their name, and `repo_path` confines a stored path to the repository root. | none |
| `pixel-recall` | Machine-wide LLM transcript retrieval: ingests Claude Code, Codex, opencode, pi, Devin, Cursor, zcode, and Gemini transcript stores into one SQLite corpus, then serves lexical and semantic search. Demand-driven: nothing scans transcripts until a recall query runs — the in-process path then catches up per agent (last week cold, since-last-ingest warm, capped at 30 days; `recall index` for the full history). Owns the embedding backends (`fastembed` ONNX and pure-Rust `model2vec`, both behind features) and the `search-meaning` code chunker, which reuses the graph's tree-sitter extraction. | git, graph, index, rank |
| `pixel-session` | One-look error capture: every error from every layer lands at throw time in one structured local SQLite sink, queryable in one call. | git |
| `pixel-actionlog` | Append-only local JSONL invocation records: measured command/outcome/duration/output volume plus versioned workflow estimates; backwards-compatible `pixel action-log` reporting, and `summarize_metrics`, which the `token-savings` plugin prints. | git |
| `pixel-task` | Durable completion contracts, deterministic workflow gates, source manifests, private verification receipts, measured task trajectories, pure policy replay, and explicit controlled evaluation. | git, ops |
| `pixel-release` | `pixel check-release`: the consistency checks a release tag must pass (CLI version, `Cargo.lock` freshness, changelog cut). Pure functions over file contents. | none |
| `pixel-setup` | Idempotent `pixel setup`, `pixel setup --remove`, `pixel doctor`. The guided run at a terminal is the `setup/` module (agent detection and targets, the four-feature catalog (Token super savior, command timing and savings, on-demand daemon, AI classification), the plan) in front of the same installer, which `install_selected` limits to the agents and hook groups the plan picked. Global install: Claude and Codex task lifecycle hooks, the `pixel` skill (`skills/pixel/SKILL.md`) deployed into every configured harness's skills dir — a guided run writes only the selected agents' roots — and, when Pi is present, the Pi package (`~/.local/share/pixel/pi-package/`, declared in Pi's `settings.json`): the explicit `/pixel-impact` command, the prompt-submit brief and the metrics relay. No always-on prompt, rules file or instruction block is deployed: guidance reaches the wired harnesses as the `pixel` skill only (Copilot CLI, Amp and the generic `AGENTS.md` target get it under the cross-harness `~/.agents/skills`), and the install cleans up nothing earlier releases wrote. Where a host's configuration already exists, Gemini CLI, OpenCode, Devin and Antigravity get their brief and metrics hooks (see "Agent integration"). An accepted classify engine (interactive install or `pixel config setup`) proposes the optional helpers: the `pixel-classify` skill into every configured harness's skills dir (`skills/pixel-classify/` under `~/.claude`, `~/.agents`, `~/.codex`, `~/.cursor`, `~/.devin`, `~/.gemini`, `~/.pi/agent`, `~/.config/opencode`) and, when Pi is present, the `pixel-classify-files` tools as a second local Pi package (`~/.local/share/pixel/pi-classify/`, declared in Pi's `settings.json`). A plugin's skill (`pixel-plugin`'s `skill` key) is delivered by `pixel install` through the same API (`skill::deploy_plugin_skill`, `.claude` plus every configured root; a plugin named like a core-managed skill is refused; `remove_plugin_skill` is the uninstall side: it removes a copy only while it still matches what was delivered, and never follows a link at the skill's dir name). `--repo` skips every global step and writes only the repository's `.pixel/config.yaml` template. Backs up changed files (`<file>.pixel-bak.<nanos>-<seq>` beside each); `uninstall` keeps those backups (a user's `skills/pixel-classify/` is renamed to `<harness root>/pixel-classify.pixel-bak.<nanos>-<seq>`, outside `skills/` so the harness stops loading it) and ends on a `backups` step listing them with the quoted `rm --` (`rm -rf --` for directories) command that drops them. A run at a terminal opens with the `banner` logo card (a short logo pulse, not an animation) and ends on the `banner` summary. `pixel doctor` reports `web-search.provider` green, and says so, when `pixel-utilities` is not installed (`pixel_plugin::Available`). | proto, daemon, index, facts, git, plugin |
| `pixel-plugin` | The plugin host, API 1 (`command` capability only). `pixel-plugin.toml` manifests (`name`, `version`, `api = 1`, `description`, `run`, `capabilities`, `network`, an optional `commands` list so one binary serves several `pixel <command>` words, run as `<run> <command> <args…>`, and an optional `skill` naming one text file inside the directory; two plugins of a tier answering to one command are an error naming both, in `resolve`, `list` and `install`), lookup in order trusted repo plugin (`<repo>/.pixel/plugins/<name>/`), user plugin (`~/.pixel/plugins/<name>/`), `pixel-<name>` on PATH (an implicit manifest-less plugin), the trust store (`~/.pixel/trusted.toml`: name plus a SHA-256 over every file of the directory, so any change revokes) with `run` canonicalised and required to stay inside the directory, the network opt-in list and install sources (`~/.pixel/plugins.toml`), `add` (the library behind `pixel install`: a local directory, a shallow `git clone` through `GitRunner` of an `https://`, `ssh://`, `file://` or `git@` URL, or `github:<owner>/<repo>[@<ref>]`, whose program is fetched as a release asset with `curl`, over the file the repository ships at `run` when there is one, a launcher say; the file `skill` names is read before anything is written — a regular file, no link, at most 65_536 bytes, UTF-8 — and a declaration that fails installs nothing anywhere), `remove` (`pixel uninstall`), `installed_skill` (the text `add` delivered, read back from the installed plugin for the uninstall sweep), `list`, and the argv and environment (`PIXEL_API`, `PIXEL_REPO_ROOT`, `PIXEL_GRAPH_DB`, `PIXEL_BIN`) a plugin runs with. `Available` is the set of moved commands (`MOVED_COMMANDS`) that a plugin runs on this host, with the host's own reason for one that is installed but blocked (untrusted, not enabled): text that would name a plugin command asks it first. `read_only_invocation` says which moved commands and modes only read, for the task gate: `repo-state`, `review-changes` and the history reads always; `recall` and `list-errors` only when their first argument is a read mode (an option first makes it not a read, so an option value is never taken for the mode); `list-branches` unless `--fetch`. Home, repo root, PATH and the GitHub base URL are fields of `Host`, never read from the process. | git |
| `pixel-bench` | Criterion benches and a real-source corpus builder (gram extraction, latency, NDCG relevance). Not shipped. | index (dev: daemon, graph, proto, recall) |
| `mvp-model` | Owned here, originally copied from pixel-mvp at a recorded commit (`crates/mvp-model/ORIGIN.md`, ADR-0035 there): the row types of pixel-mvp's index (files, symbols, edges with a target or none, provenance) and the `Graph` read trait. Pure: no I/O, enforced by its own `clippy.toml`. Sources and tests are maintained here; see `docs/mvp-handover.md`. | none |
| `mvp-store` | Owned here like `mvp-model`: pixel-mvp's graph store, one SQLite file (`graph.db`) in a `.pixel-mvp/` state directory under the root it is given, with one writer behind a lock, one generation published per transaction, and a damaged or foreign file refused and rebuilt. `pixel-graph`'s `mvp_adapter` refreshes and reads it. | mvp-model |
| `mvp-index` | Owned here like `mvp-model`: pixel-mvp's indexer for Ruby and Rust. `tree` scans the work tree through git (tracked and untracked files, the commit and the dirty flag a generation names), `link::plan` diffs it against the stored generation, `extract` parses the planned files with tree-sitter into symbols and edges, and `link::link` decides every site of the generation again under each language's rules: an edge only where the rule proves its target, every other site kept unresolved. It writes nothing; its caller hands the rows to `mvp-store`. Its `git_index` (pixel-mvp's copy of git's index, ADR-0039 there) is placed by `mvp_adapter`'s refresh under `.pixel/.pixel-mvp/` (`base` `.pixel`, as `Writer::open` gets it), and the scan reads its stat cache (`scan_discovered_in`); git's own index is never written. It spawns `git` itself, outside `pixel-git`'s `GitRunner`, through its own `bounded` module: every command is killed after `GIT_TIMEOUT` (120 s) or once its stdout passes `GIT_MAX_OUTPUT` (64 MiB), the same bounds as `GitRunner`'s default timeout and enumeration cap, and the error names the command and the bound it hit. `pixel-graph`'s `mvp_adapter` runs it. | mvp-model |

Dependency rule: `pixel-proto` and `pixel-git` are leaves (so are `pixel-context` and `pixel-release`; `pixel-session`, `pixel-actionlog` and `pixel-plugin` depend on `pixel-git` only). The owned `mvp-*` foundations depend on each other and on third-party crates only, never on a `pixel-*` crate, preserving an independent index boundary; `pixel-graph` depends on them for the index (through `mvp_adapter`), and `pixel-cli` on `mvp-model` alone for the French-prompt rules and the plain-word terms of the brief (through `execution_brief/french.rs` and `execution_brief/words.rs`); `crates/pixel/tests/cli/vendored_boundary.rs` names both readers and what each may read. `crates/mvp-lexicon/` is data, not a crate: pixel-mvp's French lexicon (`fr-en.tsv`), owned here with its provenance in `ORIGIN.md`, compiled into the brief. Its `generate.py` maintains the lemma data; CI
reproduces the TSV with `test_generate.py` without the experimental checkout. `pixel-daemon` is
the integration point and is the only library crate allowed to depend on
almost everything. The CLI depends on the daemon plus whatever it needs for
commands that never touch the daemon (install, check-release).

## Command surface

Every subcommand of the built binary, one line each, in `pixel --help`
order. `crates/pixel/tests/cli/docs_drift.rs` fails when a command listed
by `--help` is missing here, or when any `` `pixel <name>` `` in README,
ARCHITECTURE, CONTRIBUTING, `docs/manual-setup.md`, the site's `website/content/docs.md` and `benchmarks.md`, or the bundled agent prompts
(`crates/pixel-setup/assets/pixel-agent-prompt.md`,
`pixel-subagent-prompt.md`) names a command the binary does not have.

| Command | Does |
| --- | --- |
| `pixel build-index` | Build (or rebuild) the text index for a directory tree |
| `pixel search-content` | Search the indexed tree with a regex pattern. |
| `pixel search-like-rg` | Native-output literal file search for automatic routing; unsupported inputs execute the original rg/grep command without modification |
| `pixel run-recipe` | Compile and execute one bounded deterministic retrieval recipe |
| `pixel search-meaning` | Semantic code search: embed a natural-language question ("how is authentication handled?") and rank files by fusing semantic and BM25 lexical ranks over symbol chunks (tree-sitter symbols with their doc comments, windows for unparsed files); tests, configuration/data and docs are weighted below code unless the question names them. |
| `pixel scope-task` | Sniper target list: task description in, closed prioritized file list out (P0 = start here, P1 = likely, P2 = droppable). `--read-only` instead returns a typed deterministic fact envelope only from a compatible running daemon; it never starts a daemon, builds/refreshes indexes, or writes the targets manifest. `--regions` also writes `.pixel/regions.json` beside `targets.json`: symbol line ranges + context reference, conservative conflict pairs with reasons, merge-order layers from the condensed call graph, and declared shared files — evidence for orchestration, never an action recommendation. |
| `pixel brief` | Print the bounded `[PIXEL:BRIEF]` evidence context for a code-related prompt in an indexed repository; optional `[path]` selects the repository, and no applicable brief prints nothing. |
| `pixel execution-brief` | Build a task-specific ordered retrieval route from scope-task evidence: one populated first command, one retry for an empty or irrelevant result, a bounded native fallback, a maximum 40-line read, and minimal validation. |
| `pixel find-symbol` | Look up symbols by name in the code graph |
| `pixel list-signatures` | All signatures in a file — the skeleton view at ~10% of Read cost |
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
| `pixel graph-shadow` | Compare this graph's Rust edges with pixel-mvp's index, or (`--lang ruby`) count what merging its Ruby answer site by site would change (both built in a scratch directory) |
| `pixel workspace` | Multi-repo registry (`.pixel/workspace.json` members add/remove/list); `--workspace` fans `impact`/`who-calls` out across registered repos with per-repo provenance |
| `pixel index-pack` | Freeze the index into one checksummed `.pxpack` bundle — CI builds once, teammates install instead of re-indexing |
| `pixel index-unpack` | Install a packed index bundle from a path or https:// URL, hash-verified, refusing to overwrite a live index without `--force` |
| `pixel reference` | Manage version-pinned reference corpora (`.pixel/reference.json`) for multi-repo analysis on top of workspaces and index packs |
| `pixel status` | Index + graph freshness status |
| `pixel coverage` | Per-language index coverage: files on disk vs files indexed, with unrecognized extensions surfaced — the "what did the index miss?" answer |
| `pixel audit` | What an agent reads to learn what the largest source files contain: each whole file against its `list-signatures` outline in tokens (bytes / 4, floored), the total and the per-file median, files changed since indexing or with no signatures left out and counted, then per-language coverage. Builds the graph on a first run only; local, read-only, sends nothing |
| `pixel prepare-repo` | Make a repository ready for agent work: index, graph, and warm daemon |
| `pixel daemon` | Manage the per-root background daemon |
| `pixel classify` | Zero-shot decision over a bounded label set. Engines: `remote` (default; an OpenAI-compatible chat completion — `--remote-preset openrouter\|ollama\|local\|deepseek\|opencode-go\|openai\|jev\|clef-ollama\|clef-cloudflare` picks the endpoint and key variable, `--remote-model` the model; probabilities are verbalized, renormalized to sum 1; `openai` instead speaks OpenAI's `/v1/decisions` wire (gpt-6-luna: a choice question built from labels and criteria, `answers[0].probabilities` are renormalized into the distribution); `jev` instead serves TypeSafe's hosted Jev decision model over its `/v1/systemone` wire with native calibrated probabilities, disclosed in `snapshot.basis`; `clef-ollama` and `clef-cloudflare` (`decide_clef.rs`) serve Cloudflare's Clef-flash decision model over the same typed wire — through Ollama's `/v1/systemone` (`http://127.0.0.1:11434`, model `clef-flash`, no key locally, `OLLAMA_API_KEY` for a remote host) or Workers AI's REST `ai/run/@cf/cloudflare/<model>` (`CLOUDFLARE_API_TOKEN` or `CLOUDFLARE_AUTH_TOKEN`, account from `CLOUDFLARE_ACCOUNT_ID` or a stored base ending `/accounts/<id>`), with `snapshot.provider` naming the preset and the basis disclosed) or `ollaya` (`--engine ollaya --ollaya-url …`: a local Ollaya decision daemon's native typed-choice readout, TypeSafe-compatible `/v1/systemone`, with calibrated confidence disclosed in the snapshot). Remote keys resolve in disclosure order: the preset's env var, then `pixel config remote-key <preset>`, then an Infisical project when `INFISICAL_TOKEN` and `PIXEL_INFISICAL_PROJECT_ID` are set (self-hosted via `PIXEL_INFISICAL_URL`; read once per invocation, never stored in the Pixel config). Without `--engine` the stored preference decides (`pixel config classify-engine local\|remote\|jev\|clef-ollama\|clef-cloudflare\|auto`, or `pixel config remote-preset <preset> [--model --base]` for the provider, model and endpoint; `pixel setup` proposes Local/Remote/OpenAI/Jev/Clef-flash with each option's accuracy — OpenAI takes the platform key and runs the Decisions wire; Jev takes an OpenCode Go or TypeSafe key and offers that source's model pair; Clef-flash asks for an Ollama or a Cloudflare host), probing the local daemon and falling back to remote. Model-engine output discloses `snapshot.deterministic=false` and `snapshot.provider`; a `--history` accept instead names the verified-history tier with `snapshot.deterministic=true` and no provider, since a verified match is not a model output. `--context` keeps shared framing out of the state. State, context and criteria are capped separately with disclosure. Without `--label`, the local engine answers the default question battery (Ollaya's `triage` preset: `choice`/`score`/`noul` typed answers) instead of a single decision — the remote engine still requires labels. No pixel daemon; `--jsonl` serves one decision per stdin line. `--task-intent` judges the text with the built-in coding-task labels (bugfix, feature, refactor, investigate, question, review, ops) and adds the fitting pixel ops (`next:` / `next_ops`); `--if-warm` answers only from a local engine already listening — never starts it, never falls back to remote — and otherwise exits 1 with empty stdout. `--debug` asks every configured engine (local, the remote chat preset and Jev) the same labeled spec in parallel and reports each one's probabilities or its error row; `--remote-model` and `PIXEL_REMOTE_*` reach only the selected preset's lane, so a Jev lane beside another preset resolves Jev's own key, base and model. The legacy/manual Claude prompt hook asks the same `--task-intent` question itself (local daemon warm, `classify.enabled` on, 300 ms cap) and adds the verdict to the `[PIXEL:TASK_RUNTIME v1]` packet as a classifier claim |
| `pixel classify-eval` | Offline go/no-go evaluation of the verified-history retrieval tier against frozen baselines. Reports per-label precision/recall, macro scores, confusion matrix, accepted-coverage vs error curves with Wilson confidence intervals, and system metrics (fallback rate, model calls avoided, measured tier and modeled end-to-end latency, storage, error rate). The evaluation never calls a model, so the fallback path is modeled from the `--model-error-rate`/`--model-latency-ms` assumptions and disclosed as such in the report's `epistemics`/`snapshot` envelope. A pre-registered error bound and minimum useful coverage determine the verdict, and a tier that meets those bounds is still held to no-go when the strongest frozen baseline already answers with lower error at equal or better coverage |
| `pixel classify-history` | Manage the verified-history store: `list`, `add`, `remove`, `correct`, `clear`. Only human/independently verified labels are stored; model predictions cannot self-certify |
| `pixel review-gate` | Deterministic pre-review: `what-changed` plus the mechanical rules — credential-shaped added lines, and changed symbols whose callers were not themselves changed — each finding carrying its witness; the caps it fired ride the epistemics envelope |
| `pixel find-code` | Engine 1: resolve a phrase to code via the concept index |
| `pixel setup` | Set pixel up for the agent CLIs on this machine (or in one repository with `--repo`). In a terminal (stdin and stdout, no `CI`, none of `--yes`/`--json`/`--print`) it is the guided setup: detect the agents, ask which to write for and which features to switch on, review every skill file, hook step, setting and note, and write nothing until one confirmation; cancelling writes nothing. Otherwise it deploys without asking: the `pixel` skill, Claude and Codex task lifecycle hooks, Pi's package when configured, the brief hooks of the other configured hosts. `--print` renders the skill the default answers deploy and changes nothing; `--remove` removes everything either path wrote: the skills, the hook entries from the settings files, the Pi package, the brief probes, and the pixel binary itself |
| `pixel install` | Install a plugin into `~/.pixel/plugins/<name>/` from `github:<owner>/<repo>[@<ref>]` (a shallow clone; the program `run` names comes from the release asset `pixel-<name>-<target-triple>`, checked against `<asset>.sha256` (a release without one is refused unless `--allow-unverified` is passed; `--json` reports the asset URL, its sha256 and `verified`) and written beside `run` before it replaces what the repository ships there; without a usable asset a shipped program stays and the `cargo install --git … --force` command that replaces it is printed, and an unshipped one leaves the plugin not runnable with the `cargo install --git` command printed), a git URL or a local directory; a manifest's `skill` file is copied into `.claude` and every existing configured harness's skills dir once the plugin is in place, each destination printed; `npm:<package>` exits 2 with a "not supported yet" reason |
| `pixel uninstall` | Delete an installed plugin and forget its source and network opt-in; the harness skill copies it delivered are removed where they still match what was delivered, kept and reported where they differ |
| `pixel check-release` | Check that a release tag is consistent with the tree before anything is built or published: crates/pixel/Cargo.toml carries the version, Cargo.lock is fresh for every workspace member, CHANGELOG.md has the `## [x.y.z]` heading and an empty Unreleased section. |
| `pixel self-update` | Rebuild the binary, stop the daemon, copy the new binary to the install path (staged beside it, checked for length and an ELF/Mach-O header, fsynced, renamed over it, then the directory fsynced; a symlinked install path is refused), and optionally restart the daemon. |
| `pixel doctor` | Health check: install state, daemon, index/graph/facts freshness |
| `pixel run-hook` | Hook entry points `task-event` and `metrics`, invoked by Claude Code, Codex, Devin, Gemini CLI and Antigravity registrations; a verb it does not know (one an earlier release registered) prints an error on stderr and exits 0 |
| `pixel config` | Show effective settings and their sources; `setup` offers guided terminal configuration (also offered on interactive global install); `classify on\|off` controls the global classify kill switch (disabled by default); `classify-engine` stores the engine preference (`local\|remote\|jev\|clef-ollama\|clef-cloudflare\|auto`), `remote-preset <preset> [--model --base]` the provider it runs, `remote-key <preset>` its key. Global `~/.pixel/config.yaml`, repository `.pixel/config.yaml`; YAML only. |
| `pixel task-state` | Begin, contract, prepare, verify, review, finish, status, events, route, replay, evaluate, cancel and recover a durable task; show retains the Claude packet interface |
| `pixel plan` | Deterministic todo list generation from code analysis; emits blocking verification gates (auth session, env keys, provider keys, real DB state) detected in the plan's file set; persists findings in `.pixel/plan.json` so `--status`/`--done N`/`--undone N`/`--prune` track execution state across re-plans without the daemon |
| `pixel prompts` | Read the central per-user prompt store (`~/.local/share/pixel/prompts/<project-id>.jsonl`, one file per project, project-id the xxh3 of the canonical root) grouped by project: the root, the record count and the last `--limit` decisions; `--json` emits the grouping as a structure |
| `pixel plugin` | List, enable and trust plugins: `list`, `enable` (network plugins), `trust` (repo plugins); `pixel install` and `pixel uninstall` add and remove them. An unknown subcommand runs the plugin of that name (see "Request path from the CLI") |
| `pixel completions` | Print a shell completion script (`bash`, `zsh`, `fish`, `elvish`, `powershell`) to stdout, generated by `clap_complete` from the live clap tree |
| `pixel man` | Print the `pixel(1)` man page (roff) to stdout, generated by `clap_mangen` from the live clap tree |
| `pixel help` | Print this message or the help of the given subcommand(s). |

## On-disk state

Per repository, under `.pixel/` (git-ignored):

| Path | Owner | Contents |
| --- | --- | --- |
| `base.shard`, `delta.shard`, `state.json`, `build.lock` | `pixel-index` | Base shard for all tracked files at a pinned commit, delta shard for files changed between that commit and HEAD, and `state.json` as the delta-layer sidecar (tombstones for superseded base paths). The dirty working-tree overlay is in memory only. First process to hold `build.lock` builds; others wait. |
| `graph.v2.db`, `graph.lock` | `pixel-graph` (the lock: `pixel-daemon`'s `GraphLock`) | SQLite: files, symbols, edges with resolution tier. Built lazily on first graph command. Every write (a full rebuild and its rename over `graph.v2.db`, an incremental update, a watcher batch) holds `graph.lock`'s `flock`, so a daemon and a `--no-daemon` run take turns; a watcher batch skips the update when another process holds it, and the next graph op repairs the drift. A graph built with `PIXEL_GRAPH_SOURCE_RUBY=hybrid` also holds `ruby_hybrid_edges` and `ruby_hybrid_unresolved`, the undo of the Ruby merge. The name moves with the schema (`pixel_daemon::api::GRAPH_DB_FILE`); user-facing messages still say `graph.db`. |
| `.pixel-mvp/` (`graph.db` + journals, `index.lock`, `.gitignore`, `checked`, `git-index.<key>`) | `mvp-store`, through `pixel-graph`'s `mvp_adapter` | pixel-mvp's index of the Rust and Ruby files, written when the graph's root holds a `Cargo.toml` and the Rust source switch is not `PIXEL_GRAPH_SOURCE_RUST=v1`, or when `PIXEL_GRAPH_SOURCE_RUBY=hybrid` (see "Indexes and freshness"), and refreshed by every graph build and update; rebuildable, one writer at a time behind `index.lock`, a damaged or foreign `graph.db` rebuilt. `checked` stamps the files a whole-file check passed (`mvp_store::checked`): readers skip that check while the stamp holds, the writer only while the check behind it is under a day old (`TRUSTED_FOR`), and a failed run forgets it. Opened only after `pixel_git::sidecar::check`. |
| `history.db` (+ `-wal`, `-shm`, `history.db.lock`) | `pixel-facts` | SQLite: commit facts, diff text, lifecycle, FTS5 trigram indexes. Populated by `pixel build-index --history` or the daemon ingest thread on the first history query; capped by the window and budget above, with `auto_vacuum = INCREMENTAL` so an eviction shrinks the file. Schema version `FACTS_SCHEMA_VERSION` (3) in `PRAGMA user_version`: another version is rebuilt, except 2 (`UPGRADES_IN_PLACE_FROM`), whose dates are repaired in place. |
| `code-vectors/` (`manifest.json`, `seg-*.vec`, `lock`) | `pixel-recall` | `search-meaning` chunk vectors, keyed by the xxh3-128 hash of the chunk text seeded with the model id, embedder revision and `CHUNKER_VERSION`; stored as the model's `f32`s. Written only when the search root carries `base.shard` and is not `$HOME`, never by the daemon's semantic fallback. `flock` on `lock` (shared to read, exclusive to write), segments immutable, the manifest replaced by rename; rewritten with the live rows once unreachable ones exceed a quarter of them. |
| `actions.jsonl` | `pixel-actionlog` | One line per invocation, with the route and phase timings of each request it served (`serve`). |
| `brief-decisions.jsonl` | CLI `brief` (`execution_brief/decision_log.rs`) | One JSON line per prompt the prompt-submit brief judged, or declined for being about nothing: time, signal (`strong`, `weak`, `prose`), gate (`open`, `closed`, `denied`, `unjudged`, `declined`) and its reason, the gate's tier (`high`, `low`, `off`), score and model source, the best file and the features the score read, the intent judge's verdict, ops, bytes (and whether it carried the search receipt and answer excerpts, with the excerpt bytes, and for a region pack its bytes, hook parts, symbols, located blocks and whether the text search stood in for a stale graph) and milliseconds spent, whether the packet was cut short (`cut`: the chain lost evidence to the window or the op cap; a hook's probes running past the window on their `PROBE_FLOOR` after a complete chain is not a cut, and a probe its deadline ended is that probe's `timeout`), the window it was given (`deadline_ms`) and the milliseconds of each stage (`stages`: `search_ms`, `rank_ms`, `render_ms`, `probe_ms`, `null` for a stage that did not run), the evidence route (`daemon` or `local`) and what the daemon start the prompt gave came to (`launched`, `running`, `skipped: …`, `launch failed`), the task text and its SHA-256 (credential shapes masked and cut at 600 characters; when the prompt contains only a pasted block, this can include the bounded excerpt used as its plain-language task). The last 500 lines, mode 0600, never creates `.pixel/`; `PIXEL_BRIEF_LOG=0\|false\|off` writes nothing. `pixel brief --json` prints the same record beside the brief. |
| `brief-feedback.jsonl` | CLI `run-hook task-event` (`execution_brief/feedback.rs`) | What the agent read after the brief, per host session (the id is stored as 16 hex digits of its SHA-256). `prompt`: one line per prompt the brief saw, with the brief's bytes, gate and kind, the repository paths it injected (best first, at most 32) and the SHA-256 of the typed text that joins it to `brief-decisions.jsonl`. `part`: the bytes each prompt-submit hook part printed to the host, and whether the part gave up waiting for the brief (`late`), so a brief that never arrived is told from an empty one. `tool`: one line per `PostToolUse` or `PostToolUseFailure` with the tool name, whether it changed a file, and the repository files it named (the `file_path`, `path` or `notebook_path` of its input and the existing files among the words of its shell command; a file outside the repository, a link out of it or a credential-shaped path is never named). `stop`, and with it a `turn` line that closes the prompt: injected, read and edited paths, their overlap and the tool calls before the first answer. Paths and counts only, never prompt, command or output text. The last 1,500 lines, mode 0600, written through `decision_log::append_with` (never through a link, never creates `.pixel/`); `PIXEL_BRIEF_LOG=0\|false\|off` writes nothing. `scripts/bench/brief-feedback-summary.py` turns the `turn` lines into the zero-call rate and the injected-versus-read precision and recall. |
| `brief-sessions/<session>.json` | CLI `run-hook task-event` (`execution_brief/sent.rs`) | The line ranges the region packs already sent to one host session, so the next pack leaves them out and spends its room on source the session has not got. The name is the first 16 hex digits of the SHA-256 of the host's session id; the file holds keys only: per range the first 16 hex digits of the SHA-256 of the path, its first and last line and when it was sent. A key older than 24 hours is forgotten (every write also deletes session files nobody has written for a day), at most 1,000 are kept, and `SessionStart` with source `compact` or `clear` deletes the file (a `resume` keeps it: the context is the same). Written atomically with mode 0600 in a 0700 directory, never through a link (`.pixel` or `brief-sessions` as a symbolic link is refused), and never creates `.pixel/`. `PIXEL_BRIEF_DEDUPE=0\|false\|off` sends everything again. |
| `brief-parts/<session>-<prompt>.txt` | CLI `run-hook task-event --part` (`brief_parts.rs`) | The whole prompt-submit brief of one prompt, stored by part 1 of a brief split over several hooks and read by parts 2 to N (see "Agent integration"). The name is the first 16 hex digits of the SHA-256 of the host's session id, a dash and the SHA-256 of the prompt, so no host text reaches a file name. Written atomically with mode 0600 (`pixel_git::nofollow::write_replace`), never through a link (`.pixel` or `brief-parts` as a symbolic link is refused); an empty file means the prompt got no brief, so the other parts do not wait. Part 1 deletes the entry before it computes the brief, leaves an empty `.done` file with the same stem once its own output is written, and every part-1 write deletes the entries older than one hour. |
| `reconcile-conflict.json` | `pixel-ops` | Conflict marker `reconcile` writes and clears (no hook reads it). |
| `calls.json` | CLI | Circuit breaker counters for repeated identical calls. |
| `task-runtime.json` | CLI `task-state show` and Claude hooks | Existing bounded Claude context packets; independent of completion evidence. |
| `tasks/<id>/journal.jsonl`, `tasks/<id>/task.json`, `tasks/<id>/lock`, `tasks/<id>/run-<run_id>.json` | `pixel-task` | Authoritative checksummed task transactions and a rebuildable view, serialized by task lock; verification leases record child ownership for interruption recovery. Legacy v1 records migrate as unverified. |
| `tasks/source-manifests/<content_id>.json`, `tasks/source-cache.json`, `tasks/session-locks/`, `tasks/session-context/`, `tasks/route-locks/`, `tasks/enforced-sessions/` | `pixel-task` and task bridge | Immutable source manifests, metadata digest cache, atomic provider/session task binding, bounded latest prompts for first-edit activation, per-decision classification locks, and one empty marker per provider/session whose task recorded enforced gates: when the ledger cannot answer within the hook deadline, only a marked session or `task.enforcement: enforce` keeps edits and completion denied. |
| `task-hook-observations.json`, `task-hook-observations.lock` | CLI task bridge | Last real host invocation per provider; doctor reports observed activity separately from installation and trust. |
| `plan.json` | CLI `plan` | The persisted `pixel plan` checklist, so `--status`/`--done`/`--prune` survive a re-plan. |
| `workspace.json` | CLI `workspace` | The registered member repositories. |
| `plugins/<name>/` | `pixel-plugin` | Repo plugins: a directory with `pixel-plugin.toml` that someone (or the repository) put there. Read only (its manifest without following a link or blocking, at most 16 KiB), and run only after `pixel plugin trust <name>` recorded the digest of its files in `~/.pixel/trusted.toml`. |
| `config.yaml` | CLI `config` | Repository-level settings over `~/.pixel/config.yaml`. |

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

- `~/.pixel/setup-receipt.json`: global CLI setup’s private (0600), atomically
  replaced ownership receipt. It stores original bytes/modes of rewritten files,
  fingerprints of installed files and which output directories setup created.
  It covers the guided and unattended paths, including global YAML settings;
  previews and repository installs write no receipt. Removal verifies every receipt
  path against setup's output paths before changing anything, restores/deletes
  unchanged files and prunes only created empty directories. Edited files stay
  in place with a yellow report step; older installs without a receipt use the
  legacy marker-based removers.

- `~/.pixel/plugins/<name>/`: the user's plugins (`pixel install` copies
  one here; `pixel uninstall` deletes it). `~/.pixel/trusted.toml`: one `[[trusted]]`
  record (`name`, `sha256` of the plugin directory) per trusted repo plugin,
  replaced by `pixel plugin trust`. `~/.pixel/plugins.toml`: `enabled`, the
  network plugins the user allowed, and `[sources]`, where each plugin was
  installed from (a URL's userinfo removed). All three are written by
  `pixel-plugin` through a temporary file renamed into place.
  Install copying pins the chosen source root and opens children relative
  to directory descriptors through `pixel-plugin::directory` (rustix).
  Directory and regular-file opens refuse links and compare device/inode
  with the listing; link text is read relative to the same pinned parent.
  Trust hashing uses the same traversal and retains one descriptor per
  source directory through its globally sorted read. The digest byte
  encoding is unchanged, so unchanged plugins retain their trust records.
- `~/.local/share/pixel/flows/`: saved flows, written by the `pixel-flow`
  plugin (`$PIXEL_FLOW_DIR` overrides); the core only reads it (`plan_cmd::flow_dir`).
- `~/.local/share/pixel/pi-package/`: the Pi package `pixel setup` writes
  (`package.json`, `extensions/pixel-impact.ts`, and `extensions/pixel-brief.ts`, which carries the
  prompt-submit brief and the metrics relay) and declares in the
  `packages` list of Pi's `settings.json`; `pixel setup --remove` removes both.
- `<harness root>/skills/pixel/SKILL.md`: the `pixel` skill (`crate::skill::PIXEL_SKILL`),
  under `~/.claude` and each of `~/.codex`, `~/.cursor`, `~/.devin`, `~/.gemini`,
  `~/.pi/agent` and `~/.config/opencode` that exists (`SKILL_ROOTS`); a guided
  run writes only the selected agents' roots, and Pi's copy comes from its
  package step. A differing copy is backed up first; `pixel setup --remove`
  deletes it.
- `~/.local/share/pixel/pi-classify/`: the optional classify tools'
  Pi package, written and declared like the one above only when the
  classify helpers are accepted; `pixel setup --remove` removes both.
- `~/.local/share/pixel/recall/` and `~/.local/share/pixel/models/`: the
  recall corpus (`$PIXEL_RECALL_DIR` overrides) and the embedding models,
  downloaded once from Hugging Face on `pixel recall setup` or
  `pixel search-meaning`; the `--scope hybrid` search's
  `potion-code-16M-v2` is downloaded by the daemon only when it runs with
  `PIXEL_EMBED_DOWNLOAD=1`, and each potion download prints the repository,
  revision and destination first. Hybrid reuses the model cached by
  `search-meaning` without restarting. `status.embedding` exposes its model,
  resident state, completed semantic-search count, pending download and
  latest load/download error, including for detached daemons.
  Code search selects `potion-code-16M-v2` independently of transcript
  preferences. Its offline opener reads cached model bytes directly and
  refuses missing files without contacting Hugging Face.
- `~/.local/state/pixel/sniper/<project key>/`: the `pixel-session` error sink
  (`errors-v1.sqlite` plus a `project.json` naming the root);
  `$PIXEL_SNIPER_STATE_ROOT` overrides the state root.
- `~/.local/state/pixel/` (`$XDG_STATE_HOME/pixel`): `pixel-ops` crash-safety
  state, keyed by a sha256 of the repository: `journals/`, `snapshots/`,
  `publish-recovery/` and `locks/<hash>.lock/owner.json` (the lock alone is
  keyed by the canonical git common directory, so every worktree shares it). Guarded git
  mutations write nothing under `.pixel/` except `reconcile-conflict.json`.
- `~/.local/state/pixel/pr-swarm/`: the repo-local `pr-swarm` reconciler's
  state — `reconcile.log`, `lock/`, `resolve.tsv`, `last-run.jsonl` and
  `watch.pid` (the `watch` loop's claim, the authority on whether one runs) —
  written by `scripts/dev/pr-swarm.sh`, not by any `pixel` crate
  (`.agents/rules/pr-swarm.md`). The pane set itself is derived from rmux
  pane titles, not from these files.
- Daemon socket and pid: `$TMPDIR` on macOS, `$XDG_RUNTIME_DIR` on Linux
  (else `~/.cache/pixel/sockets/`), named
  `pixel-<xxh3 of canonical repo path>.sock`, `.pid` and `.lock`.

### Local usage journal provenance

`actions.jsonl`, `brief-decisions.jsonl` and `brief-feedback.jsonl` carry an
additive `provenance` object (`usage_journal.rs`): `schema_version: 1`,
`pixel_version`, `build_commit`, `build_dirty` (null when unknown), `source`
(`cli`, `plugin`, `brief`, `hook`) and `invocation_id`. The identity is shared
by all records produced by one CLI process, including its brief worker
threads; action records retain the same top-level `invocation_id` for existing
readers. Feedback may have several event kinds per invocation: deduplicate
those by invocation and `t`, not invocation alone. A turn summary's provenance
identifies the Stop process; its existing `prompt` and `session` identify the
turn it summarizes. Existing task/attempt/span correlations remain unchanged.

Native hooks bind their explicit provider and SHA-256 session pseudonym
(the same first 16 hex digits as feedback) before starting workers. Ordinary
CLI and plugin invocations use `CODEX_THREAD_ID` when present, hashed identically;
otherwise provider/session are absent. An explicit hook payload takes precedence
over inherited Codex environment. No session is inferred from the latest task
or log line. A decision can be joined to feedback by invocation even when no
session is known. Older records without provenance remain readable and their
build/context is unknown, never attributed to the current binary.

This is partial usage observation, not an exhaustive audit: hooks exit through
their own feedback path, standalone `brief` writes only its decision log,
native search fallback writes no action, and help,
argument-parse errors or killed processes may have no record. Core, plugin and
Pixel-backed compatibility-search actions use the existing bounded 300 ms
`finish_flush` before exit; disk errors or a timeout may still lose records.
Retention and opt-outs are unchanged. No raw host ID, new prompt/output content
or network telemetry is added. Producers are the three CLI action paths, brief
`Record::line`, and feedback's prompt/part/tool/stop/turn serializers. Consumers
are the telemetry plugin, hook metrics, retrospective scripts and task analysis;
all existing fields keep their meaning. The durable task journal has its own
versioned contract and is not rewritten by this observational metadata.

## Daemon and wire contract

`pixel-daemon` exposes one function that matters: `Service::handle(Op) ->
Envelope<Value>`. The Unix-socket daemon reads one JSON `Op` per line and
writes one JSON `Envelope` line back. The thread that calls `run_corpus` is
the writer: it owns the `Service` and runs one `Job` at a time (`Apply` a
watcher batch, `Serve` a request, the watch callbacks, a sweep). The loop runs
on its own thread with the socket, the watcher, the debounce and the idle and
root checks; an accept thread gives each connection a thread (at most
`MAX_CONN_THREADS`, 32) that reads its request lines and passes each one to
the loop through the same channel as the `notify` events. The loop queues
the debounced batch ahead of the requests that follow it, so the writer
still serves everything in arrival order. Shutdown and idle exit close the
queue and wait for the writer to finish what it holds. A corpus whose `ping`
answer never changes gives it through `Corpus::pong` (the repository and
recall services do): the loop answers `ping` itself, so a liveness probe
never waits behind a long request. While the writer is busy, a read-plane
request (`plane::plane`: `search`, `targets`, `resolve`, `impact`, `meaning`)
goes to one of `READERS` (2) reader threads, each holding a
`Service::read_replica`, unless a watcher batch is queued or running ahead of
it: a read never overtakes an `Apply`, so a request that follows an edit sees
it. A replica answers from the generation last published (`read_answer`
checks it before and after, inside one read transaction on the graph); it
never builds the graph, adds no git churn signals and no semantic fallback,
and runs no git: its `snapshot` is the compact `PublishedSnapshot` the
writer published with that generation (from the snapshot it attached to an
answer, staged by a watcher batch, or computed before a repository write
starts). Each successful watcher batch publishes a compact snapshot with its
new generation, so consecutive edits need no intervening writer request.
A HEAD move or a repository write withdraws it, and `Job::refreshes` keeps
reads from overtaking the batch or the HEAD move. The replica hands the
request back to the writer when the graph is missing or lags, no snapshot
describes its generation, or the generation moved. A `targets_facts`, `search`, `resolve`, `meaning`, `symbol`, `skeleton`, `context`, `uses`, `impact`,
`trace` or `evaluate` request that sets `stale_ok` (`Op::stale_ok`) trades that ordering for
latency: `Busy::reader_may_take` sends it to a reader while a batch is queued
or running too (`plane::replica_may_answer`; write-plane ops stay `Plane::Write`
everywhere else), and the replica answers from the generation before the
batch. On a replica `targets_facts` compares the index with the HEAD of the
published snapshot and takes the graph's stored signature
(`published_signature`) instead of walking the tree. When `Busy::behind`
reports an unfinished batch once the answer is computed, `stamp_stale` sets
`epistemics.staleness_ms` to how long the oldest one has been queued (the
debounce before it is not counted) and adds a `STALE_GENERATION` warning
naming the generation served. `evaluate` on a replica uses the published graph identity and snapshot semantics,
without a working-tree freshness sweep or refresh. `context` still verifies
source hashes and omits changed excerpts instead of combining old spans with
new text. `search` uses the published candidate index but scans current file
contents: the generation label does not promise historical source bytes.
Without the flag nothing changes; a daemon
that predates it ignores the field and answers after the batch, so a client
reads the envelope, not its own flag, and `PROTOCOL_VERSION` stays. The watcher debounces
filesystem events and refreshes the index and graph for changed files
(`Service::refresh_files`) without stopping the replicas: it stages the
index batch under the index's read lock (`IndexSet::stage_refresh`: the
policy verdict and the disk reads) and writes the graph rows in their open
transaction, which SQLite readers do not see. Only `Swap::open`, run by
`pixel_graph::build::update_files_gated` right before the `COMMIT`, takes
the publication's write lock: it applies the staged index
(`IndexSet::apply_refresh`, in memory), then the graph commits and the
generation moves. A replica reading meanwhile answers from the previous
index, rows and generation. Only
events admitted by `record_event` extend the 500 ms quiet window; the first
pending event caps queue wait at `MAX_PENDING_WAIT` (2 s), so ignored-tree
traffic and continuous edits cannot postpone the timer forever. Applying the
batch can take additional time. Both a timer flush and a pre-request flush
clear the first-event timestamp. The watch is
registered on its own thread so the daemon answers from its first request.
On macOS it is one recursive FSEvents watch per root, which walks nothing.
Elsewhere (`register_pruned_watch`) it is one non-recursive watch per
directory whose name is not in `IGNORED_DIRS`, plus `<root>/.git`, `.git/info`
(its `exclude` file) and `.git/logs` (the `HEAD` reflog): a recursive inotify
watch walks and watches every ignored directory too (15 825 watches where
2 216 suffice) and fails past `max_user_watches`. A directory created or moved
in later is added to the watch by the loop, and the files already inside it
join the pending batch. A write to `.git/HEAD`, its reflog, `packed-refs` or a
branch ref reaches `Corpus::git_moved`, which drops the `Service`'s cached
HEAD and branch; while the watch covers `.git` those live up to
`WATCHED_REF_TTL` (60 s) instead of `SNAPSHOT_CACHE_TTL` (1 s), and a watcher
batch drops only the cached dirty list. `status` reports the watch under
`watcher` (`ready`, `watched_dirs`, `recursive`, `registration_ms`,
`git_dir`). Once it is live, `Corpus::watch_ready` re-reads every path `git status`
lists, every path the index overlay held (an edit discarded meanwhile leaves
`git status` clean) and every path a HEAD move since the open changed, since
edits made meanwhile raised no event. The daemon exits after
thirty minutes idle; the brief's hooks start it again when they find none
(see "Agent integration").

A panic in a request handler is caught around `Corpus::handle` and becomes a
failure envelope (`request panicked: <message>`) for that one request; the
index and publication locks it may have poisoned recover on their next
acquisition (`api::RecoverLock`). Both stay consistent through a panic:
the index is swapped by one assignment or an in-memory batch, and a
publication is marked unhealthy before work that can fail. A panic outside a request still ends
the loop, and `ReleaseOnExit` then removes the socket, pid and lock files.

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
agree, using the variants supplied by Serde's derived deserializer).
The capability registry contains wire tags, including `reindex` (sent by
`build-index`), and excludes only the internal lifecycle op `shutdown`.
`flow` is not an `Op` and is not advertised in this registry; SessionStart
gets its command list from the CLI parser instead.

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
   `daemon_auto_start: false` in the config, disables the spawn. One function
   (`spawn_daemon`) spells that spawn for this path, `pixel daemon start` and
   the hooks, and `--warm-meaning` (hidden; background start only) makes the
   start process ask the new daemon one `meaning` question once it answers.
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
   `find-code` bounds each match's inline context before that cap: 16
   lines for the first match printed, 4 for the others, then a
   `… N more lines, to <path>:<end>` pointer; a match whose span lies inside
   a body already printed points back to it. Either bound, when it fires,
   is named in the answer's `caps` (and as a `cap:` line in human output).

A subcommand clap rejects as unknown never reaches `execute`: `main.rs` asks
`pixel-plugin` to resolve the word as a plugin (trusted repo plugin, user
plugin, then `pixel-<name>` on PATH) and runs it as a child with this
process's stdio and `PIXEL_API=1`, `PIXEL_REPO_ROOT`, `PIXEL_GRAPH_DB` and
`PIXEL_BIN` set; `plugin_cmd::run_logged` waits for it, appends the run to
`.pixel/actions.jsonl` like a core command (arguments through `logged_args`,
the outcome as `<name> exited <code>`, the duration; no metrics, since the
core does not see the plugin's output), and exits with the plugin's own code
(`128 + signal` when a signal ended it). An untrusted repo
plugin, a network plugin not enabled, or a `run` that leaves its directory
stops with exit 1 and the command that fixes it; a name in
`pixel_plugin::MOVED_COMMANDS` that no plugin provides prints the
`pixel install github:Pixel-CLI/<repo>` hint and exits 2; a name in
`removed_commands::REMOVED_COMMANDS` (the 41 pre-rename aliases 0.8.0
removed) prints ``'<old>' was removed in 0.8.0; use `pixel <new>` `` (with the
install command when `<new>` is a plugin command no plugin provides) and
exits 2, without running anything; any other name keeps clap's error. That table maps the 30 commands (#878) that live in the plugin
repositories `pixel-git`, `pixel-flow`, `pixel-recall`, `pixel-telemetry` and
`pixel-utilities` to their repository. The core builds in none of them but
`check-release` (the release workflow runs it before any plugin could be
installed), so the hint is reached for the other 29. The core keeps what other commands read:
`workspace.json` for `--workspace`, the flow store for `plan`,
and the library crates (`pixel-ops`, `pixel-facts`, …) the
plugins link at a pinned revision. The flow libraries (`pixel-flow`,
`pixel-ultraflow`) live in the `pixel-flow` plugin repository itself.

So the CLI's `--json` output is the envelope's `result` with the honesty
fields merged in, not the raw envelope. Anything that needs the full
envelope talks to the daemon socket directly.

## Indexes and freshness

- The text index is git-anchored: base shards correspond to a commit, delta
  shards to changes since, and an overlay covers the dirty working tree.
  `pixel status` reports whether each layer is fresh. A watcher batch
  decides which paths the overlay admits from the walk policy's verdict
  kept on `IndexSet`: the first batch and any batch that touches an
  ignore-control file (`.gitignore`, `.ignore`, `.git/info/exclude`) run the
  full `policy_walk`; any other batch judges a new path by a walk from its
  nearest admitted ancestor (`index::policy_admits_from`).
  The set also keeps the HEAD its content covers (`IndexSet::covered_head`):
  the HEAD at open (base ∪ delta answer for it), moved by
  `Service::catch_up_head` once every path between it and a new HEAD is
  refreshed. The daemon runs that catch-up when a ref moves (`git_moved`),
  after its watch registers (`watch_ready`) and before each evidence bundle;
  a move wider than `HEAD_CATCH_UP_MAX_PATHS` (2 000) or a failed diff
  leaves it behind. `targets_facts` answers `index_stale` only while
  `covered_head` differs from HEAD, never on the base shard's commit,
  which only a rebuild moves.
- The graph is built lazily on the first graph command and updated per file
  by the daemon watcher. Its quiet debounce extends only for recorded content
  events and is capped from the first pending edit (see Daemon and wire contract).
  Without a daemon (CI, `PIXEL_DAEMON_AUTO_START=0`,
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
  A call this resolver leaves unresolved to a name no symbol carries
  (outside Ruby files and import bindings, whose rules reach a callee the
  call does not name) is recorded once per file and name in
  `unresolved_names` and counted on its enclosing symbol
  (`symbols.unnamed_calls`, read by `uses … callees`), not per site in
  `unresolved_calls`; an update whose batch defines such a name reads the
  files listed for it again, as if they were in the batch, so it links
  what a full build links.
- The Rust source switch, `PIXEL_GRAPH_SOURCE_RUST` (`v1` off; unset or
  anything else on, the default; `pixel_graph::mvp_adapter::RustSource`),
  read by the process that builds the graph (the daemon, or the CLI
  without one). With it on and a `Cargo.toml` at the graph's root, the
  full build and every incremental update (`write_rows`, so
  `update_files` and `apply_tree_delta`) refresh pixel-mvp's index in
  `.pixel/.pixel-mvp/` and replace the Rust rows with its answer
  (`mvp_adapter::merge_into`): every `calls` edge from a Rust symbol
  (all `exact`: pixel-mvp links a site only where Rust's rules prove the
  target), every `references` edge (`probable`), every `unresolved_calls`
  row of a Rust file (a site whose name no symbol carries leaves none: it
  can stand for no symbol of the graph; it is counted on its enclosing
  symbol, `symbols.unnamed_calls`, which `uses … callees` adds to its
  unresolved outgoing sites), every Rust import's target. An
  incremental update
  writes again only the Rust files whose rows may have moved
  (`mvp_adapter::merge_again`): its own, those whose rows its passes moved
  (`Affected::rows_moved`: a demoted edge, a reconsidered one, a
  re-pointed import), those pixel-mvp's refresh rewrote or redirected a
  site of, and those resolving a site into a file whose definitions moved;
  every other file keeps its rows untouched. The graph records the
  pixel-mvp generation its Rust rows were written from
  (`rust_mvp_generation`, dropped by every full build): an index at
  another generation, or a batch that makes a name defined nowhere else
  appear or disappear (an unresolved reference to it is recorded only
  while some symbol carries it), writes every Rust file again. This resolver then
  decides no Rust site (the batch's Rust calls are not resolved,
  `resolve_affected`'s retry skips their rows) and `ResolveIndex` offers
  no Rust symbol to a caller of another language. The graph records its
  Rust source in the `graph_source` meta key (`rust=mvp <commit>
  <extractor>`; absent with `v1`, so that graph is the one built before
  the switch existed), and a graph whose source differs from the
  switch's, or from the imported commit and extractor, is not current: the
  next read rebuilds it whole. Where pixel-mvp cannot index (no
  `Cargo.toml` at the root, the root is not the top level of a git work
  tree, a tracked `.pixel/`), the graph keeps this resolver's Rust edges
  and records `rust=v1, pixel-mvp unavailable`, which counts as current
  while the switch is on. Every other language keeps this resolver; Ruby
  too, unless the Ruby switch below merges. Known limits of the Rust
  source, kept here until pixel-mvp fixes them:
  - Rust test code (`#[test]` functions, `#[cfg(test)]` modules) has no
    symbols in this graph, so its calls are dropped (`pixel graph-shadow`
    counts them);
  - a local typed only by what a function returns is not followed beyond
    `T::f(…)?`, `.unwrap()` and `.expect(…)` on std's `Result` or `Option`
    of `T`: such method calls stay unresolved, counted in the lower bound;
  - a macro invoked in a block may write a method or an impl the rules do
    not see; a dependency's attribute macro is read as inert and its
    derive may write beside its type; a dependency's own macro named
    `lazy_static` or `thread_local` is taken for the standard one; cfg-like
    forms outside `#[cfg]` are not read;
  - pixel-mvp's index spawns `git` without `GitRunner`'s timeout and output
    cap;
  - an incremental update refreshes pixel-mvp's index on a thread of its
    own while its passes write this graph's rows, and only the merges
    wait for it (`write_rows_with`): on this repository (`bb4991fe`,
    `dev-release` build) that refresh is about 0.55 s of a one-file save,
    and `.pixel/` holds about 44 MB more.
- The Ruby source switch, `PIXEL_GRAPH_SOURCE_RUBY` (`hybrid` on; unset or
  anything else off, the default; `pixel_graph::mvp_adapter::ruby_switch`),
  read like the Rust one. With it on, the full build and every incremental
  update refresh pixel-mvp's index in `.pixel/.pixel-mvp/` (any git work
  tree, no `Cargo.toml` needed) and merge its Ruby answer into this
  resolver's rows site by site (`mvp_adapter::ruby`, ADR-0037 of the
  pixel-mvp research repository): where pixel-mvp resolves a site, its
  targets replace this resolver's edges there (calls `exact`, references
  `probable`; a target with no symbol here leaves an `unresolved_calls`
  row); where it abstains or has no such site, this resolver's edge stays
  in its tier; a site only pixel-mvp has (an ancestor, a route) gains its
  edge. An edge of this resolver goes only where pixel-mvp proves another
  target, so its right edges stay, and so do its wrong ones where
  pixel-mvp abstains. The merge records its undo in `ruby_hybrid_edges`
  and `ruby_hybrid_unresolved`, each row with its site's file:
  `write_rows` restores this resolver's rows first, runs its passes as
  without the switch, then merges again, file by file
  (`ruby_switch::merge_again`), with the refresh of pixel-mvp's index that
  ran beside those passes, as for Rust. The merge is decided per file, so only the
  files whose merge may have moved are planned again: the update's own,
  those whose rows the passes moved (a multiset hash of each Ruby file's
  rows before and after them, `ruby_switch::prints`), those pixel-mvp's
  refresh rewrote or redirected a site of, and those resolving a site
  into a file whose definitions moved (`Store::files_reaching`); every
  other file gets back the rows it had. The graph records the pixel-mvp
  generation it was merged against (`ruby_hybrid_generation`); an index
  at another generation (a refresh whose graph update did not commit)
  makes the update merge every file. The graph records
  `graph_source_ruby` (`ruby=hybrid <commit> <extractor> undo.2`, or
  `ruby=v1, pixel-mvp unavailable` outside a git work tree's top level;
  absent when off, and a full build drops the side tables), and a graph
  whose Ruby source differs from the switch's is rebuilt whole. Cost on
  antiwork/gumroad (`b996417e`, `dev-release`, pixel-mvp `e91e8da`): a
  one-file edit then a graph command 0.71 s → 4.4 s (a line appended;
  2.4 s → 6.1 s for a method inserted mid-file), nearly all of it
  pixel-mvp's refresh of its index (its writer's full open check and
  every Ruby site decided again, 2.9 s); a cold build 5.6 s → 12 s;
  `.pixel/` 174 MB → 302 MB. On the daemon bench of
  `docs/bench/ruby-hybrid-daemon-2026-10-18/` (release build), with the
  refresh beside the passes, a method inserted mid-file is in the graph
  after 6.0 s p50, 7.3 s p95 (v1 2.7 s, 2.9 s), for 8.0 CPU s of the
  daemon (v1 2.2); that is why it is off by default.
- JavaScript assignment syntax is retained separately in `js_assignment_facts`
  (extractor 30). `extract::js_assignments` records byte ranges, AST context
  identities, variable/parameter patterns and write expressions, including
  chained RHS references, computed accesses and nonfunction reassignments.
  Contexts describe containment, not binding resolution or runtime receivers;
  imports and other declaration forms are outside this partial inventory.
  Invalid parses and depth cuts are explicit. Full and incremental `store_batch`
  share the writer; file replacement/removal clears old facts. The resolver,
  symbol lookup, rename and brief packs do not consume these rows: an assigned
  method name creates no new symbol or edge. Facts refer to `files.blob_oid`'s
  bytes, and the extractor-version change rebuilds older graphs.
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
  A Ruby `alias` with a parser-inserted missing name is ignored: it defines
  no method and cannot take ownership of an earlier declaration's reference.
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
- History facts are ingested by a dedicated low-priority thread. Before
  fetching diffs, retention excludes expired commits from blob checks.
  Widening the window rewinds the blob-check cursor before restored diffs
  can be read. Both sides of retained changes are measured, including
  deleted/modified blobs whose parent commit has expired. Phase B measures changed blobs across 25 commits with size-only
  `cat-file --batch-check` requests (128 objects per process). Unsupported
  filenames or failed batches fall back to bounded `ls-tree` lookups, so
  large blobs remain excluded without reading their contents. Explicit
  `build-index --history` runs print periodic commit progress on stderr and
  cumulative metadata/blob/diff/retention timings, plus total wall time;
  daemon background ingestion stays silent. Queries
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

Setup and removal edit JSON settings by key, retaining untouched values and their
formatting. Codex hook writes use the shared atomic settings writer; setup reads
Codex TOML review state without rewriting it. Gemini and Antigravity use this
writer too, taking a `.pixel-bak.*` copy before every changed configuration write.
Global CLI setup records its output paths in the private ownership receipt;
reinstalls retain the original ownership of unchanged outputs. If a user edited
an output before reinstalling, that edit becomes its restore baseline.
Removal takes back unchanged writes, including the global YAML template and newly
created hook files, while reporting and preserving files edited after setup.
Recovery copies stay in place and the report lists them, including nested skill
backups; preexisting empty directories are kept. When removal preserves edits,
the remaining receipt stays private so repeated removal cannot delete those edits
through the legacy removers. Symlinked output directories are followed without
cycles; a retargeted link preserves files at its new destination. Generated-file
writes follow complete file-symlink chains and preserve target permissions.
Snapshotting rejects directory links to ancestors and non-regular files before
setup writes, preventing a scan of the whole home or a blocking read of a FIFO.

Setup and removal edit JSON settings by key, retaining untouched values and their
formatting. Codex hook writes use the shared atomic settings writer; setup reads
Codex TOML review state without rewriting it. Gemini and Antigravity use this
writer too, taking a `.pixel-bak.*` copy before every changed configuration write.

`pixel setup` has two ways to run, and one installer behind both. At a terminal
(stdin and stdout, `CI` unset, no `--yes`, `--json` or `--print`) it is the
guided setup of `pixel-setup::setup`: it detects the agent CLIs by their
environment markers and configuration directories, asks which to write for and
which of the 4 features to switch on, resolves the answers to a `SetupPlan`
with no I/O, prints the plan (the installer's dry-run steps, configuration
keys and notes), and writes nothing until one confirmation. The apply runs
`install::install_selected` for the picked agents and the hook groups the
features name (`Task`, `Codex`, `Pi`), then stores the configuration keys. An
agent that was not picked is not touched. Without a terminal, in CI or with
`--yes`/`--json` the same command is the unattended installer
(`install::install`), which deploys for every configured host. `pixel setup
--remove` takes out what either run wrote. The plan holds no file text: the
installer owns every write, so the two paths cannot disagree on a file.

The provider-neutral task layer lives above retrieval. `run-hook task-event`
normalizes Claude, Codex and Pi events into `task_bridge`; it never changes
the retrieval daemon's wire protocol. Global Claude/Codex installation owns
one native lifecycle registration; a Pi host that drives the task lifecycle
sends its own events, and Pixel's Pi package sends only brief-only ones (see
"Agent integration").
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
Session lookup reads each journal's bounded, checksummed first transaction to
locate candidate tasks, then validates the matching tasks' complete journals.
Rebuildable `task.json` views cannot hide a session binding. Within one store
instance, validated journal transactions are reused only while file identity,
size and modification/change timestamps match; writes invalidate or refresh
that cache. No persistent lookup index or journal-format migration is required.
Blocked/retried model requests count as work; coordinator and classifier calls
are separate. Missing host/child coverage produces a lower bound, never zero
work. `task replay` reads frozen policy frames without execution. Empirical regret
compares only matching, successful, completely observed attempts. The existing
`eval/score.py` and `eval/gate.py` own quality/turn regression gates; the controlled
container backend adds isolated three-arm trials, not a replacement arena.

A global `pixel setup` deploys no agent prompt and wires no retrieval
guard: every agent keeps its native search. It cleans up nothing earlier
releases wrote (see the end of this section). It handles each agent through
its own extension point:

- **The `pixel` skill** (`skills/pixel/SKILL.md`, `crate::skill::PIXEL_SKILL`):
  written under `~/.claude` and under each of `~/.agents`, `~/.codex`,
  `~/.cursor`, `~/.devin`, `~/.gemini`, `~/.pi/agent` and `~/.config/opencode`
  that already exists (`config::SKILL_ROOTS`); a guided run writes only the
  selected agents' roots. `~/.agents/skills` is the cross-harness personal
  skills directory GitHub Copilot CLI and Amp read: the generic `AGENTS.md`
  target's root. Pi's copy comes from its package step, and Antigravity's
  plugins carry theirs.
- **Claude Code**: task lifecycle hooks, the prompt-submit brief (up to
  `brief_parts` entries) and a `PostToolUse` metrics relay in
  `~/.claude/settings.json`; task contracts and foreign hooks are preserved.
- **Codex**: the same suite in `$CODEX_HOME/hooks.json` (default
  `~/.codex/hooks.json`). Codex runs a hook only after it is reviewed with
  `/hooks`, which `install.codex-hook-review` reports.
- **Pi**: when Pi's agent directory (`$PI_CODING_AGENT_DIR`, else
  `~/.pi/agent`) exists or `pi` is on `PATH`, a local Pi package under
  `~/.local/share/pixel/pi-package/` (`package.json`,
  `extensions/pixel-impact.ts`, which registers the explicit
  `/pixel-impact <symbol>` command, and `extensions/pixel-brief.ts`, which
  runs `pixel run-hook task-event --provider pi` on `before_agent_start`
  (the prompt-submit brief), on `session_start` and `session_compact`, and
  `pixel run-hook metrics --provider pi` on the `tool_result` of a bash call
  that ran `pixel`; see the hook table) and its absolute path in the
  `packages` list of Pi's `settings.json`, which keeps every other key and
  package. A settings file that does not parse, or an agent path that is not a
  directory, is left untouched and reported yellow.
  Accepting a classify engine additionally offers the `pixel-classify` skill
  (`skills/pixel-classify/SKILL.md` under the agent's config dir) and the
  `pixel-classify-files` tools, which judge a file's contents without loading
  them into context, as a second Pixel-owned package
  (`~/.local/share/pixel/pi-classify/`, declared in the same `packages` list).
  Uninstall removes both packages and their entries.
- **Gemini CLI, OpenCode, Devin, Antigravity**: integrations are installed
  only where each host's configuration already exists. Gemini: a
  `BeforeAgent` hook in `~/.gemini/settings.json`. OpenCode:
  `plugins/pixel-brief.js` in `~/.config/opencode` (`$XDG_CONFIG_HOME`
  honoured), whose V2 `session.hook("prompt")` handler runs `pixel brief`
  and whose `context` hook adds the result to the turn's first request as a
  system part. Devin: the task
  events in its hooks file, `prompt-submit` among them; that entry alone sets
  `show_output`, because Devin draws neither `systemMessage` nor a hook's
  stdout or stderr, and its `✱ Hook completed: <command>` line is the only
  on-screen trace that the brief ran. Antigravity: a
  `pixel-brief` entry in `~/.gemini/config/hooks.json` running on
  `PreInvocation`, plus the IDE (`plugin_dir`) and CLI (`cli_plugin_dir`) Pixel plugins, each
  carrying `skills/pixel/SKILL.md` (exactly `crate::skill::PIXEL_SKILL`) and a `pixel-guard`
  `PostToolUse` hook that stores a metric in that conversation's
  `artifactDirectoryPath/.pixel-pending-metrics/<stepIdx>.json` and returns
  `{}`. Its `PostInvocation` hook consumes only that conversation's pending
  line as an `injectSteps[].ephemeralMessage` and clears the file. Unrelated
  user hooks and settings are preserved. `pixel doctor`
  (`install.antigravity`) flags a plugin skill that differs from
  `PIXEL_SKILL`.
- **Cursor**: the skill only. **Copilot CLI**, **Amp** and the generic `AGENTS.md` target: the skill under `~/.agents/skills` only.

Install and uninstall rewrite only recognized Pixel registrations and files,
preserving foreign settings and hooks.

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

The shared setup writers sync staged file bytes before renaming them over
user configuration. Gemini brief settings and Antigravity hooks and plugin
registration use the shared settings writer, including backups, symlink-target
preservation and private file permissions. Parent directories are not fsynced;
this is not a claim of complete power-loss durability.

`pixel setup --repo <path>` skips every global step and writes the repository's
`.pixel/config.yaml` template, the one file Pixel puts under a repository: it
registers no hook or guard there, and `crates/pixel/tests/cli/docs_drift.rs`
(`a_repo_install_should_write_only_the_repository_config`) holds the docs to
that. `repo.task-hook-observations` is the one `repo.*` check of `pixel
doctor`.

`pixel run-hook` exposes task lifecycle and metrics entry
points. `task-event` is registered for Claude Code, Codex, Devin, Gemini CLI
and Antigravity; the metrics relay is registered for Claude Code, Codex and
Antigravity. Pi has no hook registration: its package (`extensions/pixel-brief.ts`)
runs both entry points from Pi's extension events:

| Hook event | Command | Effect |
| --- | --- | --- |
| `SessionStart`, `UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `Stop`, `SessionEnd`, `SubagentStart`, `SubagentStop`, plus `PostToolUseFailure` (Claude) or `Interrupt` (Codex) | `pixel run-hook task-event --provider <host> --event <event>` | Binds coding objectives, gates edits, records tool outcomes, and bounds Stop correction. Global native hooks compose with existing hooks. Once enforced, a task retains its gates if runtime settings change. On `prompt-submit` it also returns the `[PIXEL:BRIEF]` evidence brief as `additionalContext` (see below). On `SessionStart` and a briefed `UserPromptSubmit` it may start the repository's daemon in the background (see "Keeping a daemon behind the brief"). Every hook of the table also leaves its line in `.pixel/brief-feedback.jsonl` after it has answered the host (`prompt-submit` parts, `post-tool-use`, `tool-failure`, `stop`): the read feedback of the brief, best effort and silent, so it adds no registration and cannot fail a host. |
| `UserPromptSubmit` (Claude Code and Codex, `brief_parts` entries; default 3) | `pixel run-hook task-event --provider <host> --event prompt-submit --part K/N` | The brief split over `N` hooks (see "A brief larger than one hook"). Part 1 is the entry above with the ledger and the brief; parts 2 to `N` only print their slice. |
| `before_agent_start` (Pi, `pixel-brief.ts`) | `pixel run-hook task-event --provider pi --event prompt-submit` | Payload `{session_id, cwd, prompt, brief_only: true}` on stdin. `brief_only` (`BRIEF_ONLY` in `task_hook.rs`) keeps the ledger out: no task is bound, nothing is gated or enforced, and the answer is the bare decision `{"decision":"observe","context":<brief>}`. The brief gets the configured `brief_parts` budget (3 by default) as one message, the 3 s hook window, and leaves its line in `.pixel/brief-feedback.jsonl` and the per-session dedupe. A delivered brief adds the `🟩 pixel prompt-submit` `systemMessage` under the same `metrics` rules as the other hosts. The package injects `context` as the `pixel-brief` custom message with the untrusted-data trailer, and shows `systemMessage` to the user only (`ctx.ui.notify`, never part of the model's context). A Pi payload without `brief_only` keeps `TaskProvider::Pi`'s ledger-only behaviour and gets no brief. |
| `session_start`, `session_compact` (Pi, `pixel-brief.ts`) | `pixel run-hook task-event --provider pi --event session-start` | Payload `{session_id, cwd, source, brief_only: true}`; `source` is `clear` for a new session, `compact` after a compaction and `startup` otherwise. It may start the repository's daemon and, for `clear` and `compact`, forgets what the brief already sent. |
| `tool_result` of a bash call that ran `pixel` (Pi, `pixel-brief.ts`) | `pixel run-hook metrics --provider pi` | Claude-shaped payload (`tool_name`, `tool_input.command`, `tool_response`, `cwd`, `session_id`). Relays the finalized Pixel metrics line as `systemMessage`, whether or not the bash result already holds the box, as Codex does; the package shows it to the user only and never rewrites the tool result. |
| `PostToolUse` (Claude Code, matching `Bash`) | `pixel run-hook metrics --provider claude` | Relays the finalized Pixel metrics line to Claude's `systemMessage` after a shell call. |
| `PostToolUse` (Codex) | `pixel run-hook metrics --provider codex` | Relays the finalized Pixel metrics line through Codex's documented `systemMessage` only when its exact identity header appears in this tool result and uniquely matches a command/cwd record. Missing or ambiguous evidence stays silent. |
| `PostToolUse` (Antigravity, `pixel-guard` in the IDE and CLI Pixel plugins) | `pixel run-hook metrics --provider antigravity` | Matches a finalized metric to the tool call, stores it in that conversation's artifact directory keyed by `stepIdx`, and returns `{}` as required by Antigravity. |
| `PostInvocation` (Antigravity, `pixel-guard` in the IDE and CLI Pixel plugins) | `pixel run-hook metrics --provider antigravity --event post-invocation` | Consumes matching pending metrics for the same `conversationId` and `artifactDirectoryPath`, injects each as one `ephemeralMessage`, and clears the stored record. |

What earlier releases wrote is not cleaned up: the managed
`<!-- pixel:setup:start -->` and `<!-- pixel:managed:begin -->` blocks in
instruction files and `developer_instructions`, rule files, the `claude()`
shell wrapper, the `guard`, `composed-guard`, `session-start`,
`prompt-submit`, `post-compaction` and `post-tool-use` hook verbs, Pi's
`APPEND_SYSTEM.md` block and project guard, OpenCode's `AGENTS.md` block and
guard plugin, and the zcode, Cursor, Copilot CLI and Warp artifacts. A hook
verb `pixel run-hook` does not know prints an error on stderr and exits 0, so
a registration left in an agent's settings cannot block or fail a host. A
panic in a hook's own work exits 0 as well: `task-event` and `metrics` run
inside `hook_input::exit_0_after`, which catches the unwind, keeps whatever
response was already printed and adds nothing, so the panic message goes to
stderr and never turns into exit 101.

### A brief larger than one hook

Claude Code caps the `additionalContext` of one `UserPromptSubmit` hook at
10,000 characters (measured: 10,000 arrive whole; 10,001 arrive as a preview of
about 2,000), and Codex spills a hook's context past 2,500 estimated tokens,
four bytes each, to a file behind a head and tail preview
(`additionalContextLimit` in `codex-rs/hooks`; the default applies). Both cap
*per hook* and run the matching hooks of an event concurrently, so a brief
larger than one hook may inject is delivered by several, and `pixel setup`
registers `brief_parts` of them (`~/.pixel/config.yaml`, default 3, 1 to 8; 1
is the single hook of earlier releases): the same command with `--part K/N`,
one entry each, in order, in `~/.claude/settings.json` and `~/.codex/hooks.json`.
Every other host keeps one hook: Gemini (`BeforeAgent`) documents no cap on
its `additionalContext`, and its source (`hookAggregator.ts`, `client.ts`)
joins and appends the contexts of its hooks without one. Antigravity
(closed source) and Devin document no cap and none was measured, so they
keep one hook too.

`brief_parts.rs` and `task_hook.rs` do the rest:

- **Part 1** runs today's path: the ledger, and the brief on its own thread. As
  soon as the brief is finished (not after the ledger) it stores the whole
  text under `.pixel/brief-parts/` keyed by the session id and the prompt
  (`cache_path`, see "On-disk state"), an empty one for a prompt with no
  brief, and prints slice 1. Native hooks have an eight-second decision limit;
  Pi keeps six seconds to stay within its shorter subprocess deadline. If
  split part 1 reaches its limit just as the brief is stored, it prints the
  stored first slice after a short grace period, so the head and its notice
  survive a late decision. Later parts leave another 250 ms for that response
  to flush and release its done marker. It says nothing about parts.
- **Parts 2 to N** read the same payload, derive the same key, wait for the
  file (polling every 15 ms) and then for part 1's `.done`
  marker, and print their slice in the same hook envelope: `{}` for a brief
  too short to reach them, nothing at all when the file does not come in
  time, the payload names no prompt or repository, or the file is not
  readable. The marker matters because the host orders what concurrent hooks
  print by when each finishes (measured: with parts 2 and 3 free to print as
  soon as the brief was stored, Claude Code put them ahead of part 1); a
  part 1 that never marks itself done costs only the remaining bound
  (`decision_timeout` plus 750 ms). A marker explicitly recording a missing
  head shifts later slices down by one: part 2 sends the first slice and its
  notice, preserving the head if part 1 gave up before a late store. They
  run no ledger, so one prompt is one ledger event.
- **Fails open.** Claude Code blocks the user's prompt when a
  `UserPromptSubmit` hook exits 2, which is what an older `pixel` does on the
  unknown `--part`. Every prompt-submit command `pixel setup` writes ends in
  ` || true` (`routing::FAIL_OPEN`; both hosts run hook commands in a shell:
  Codex as `<shell> -lc`), recognition strips it so entries with and without
  it are the same hook to install, doctor and uninstall, and `main` exits 0
  with the clap error on stderr for any `run-hook` argument it cannot parse.
- **Slicing** (`split`) is deterministic and cuts at line boundaries: a part
  takes whole lines while they fit in 9,800 characters (`PART_CHARS`; for
  Codex also 9,800 bytes, `Limits::BYTES`), and a line longer than a part is
  cut at a character boundary. The slices in order concatenate to the stored
  brief; a brief longer than `N` parts loses the lines after the last one.
- **Doctor**: `install.claude-hooks` is red and names the missing part
  (`brief part 2/3 missing`) when the set is incomplete, mixed with a whole
  entry or of two sizes, and yellow when the set is complete but not the size
  `brief_parts` asks for. `install.codex-hook-review` counts every part: Codex
  skips a hook the user has not reviewed.
- `PIXEL_BRIEF_TEST_PAD_PARTS=N` (debug builds only) grows the brief to `N`
  parts each ending in a `PIXEL-PART-MARKER-K-OF-N` line; the CLI tests and
  the live proof against Claude Code use it to reach an oversized brief.

The brief opts its daemon retrieval and graph requests into `stale_ok`:
`search`, `resolve`, `meaning`, `targets_facts`, `symbol`, `context`, `uses`,
`evaluate`/`trace`, `skeleton` and `impact`. Those requests rely on the daemon's
answer rather than a client-side graph freshness refusal. A successful
response with `epistemics.staleness_ms` (including zero) marks the shared
brief evidence as stale before its rows are published. Normal, low-tier,
partial and region-pack output retain one `staleness:` line saying that some
evidence predates the last edit. A stale pack is a lead to verify; the notice
counts against its limit and displaces whole trailing blocks if needed.
Fresh responses add no notice, and later fresh reads cannot erase an earlier
stale response's warning. Local graph readers retain their freshness checks;
`status` and read-only `history` keep their existing writer route.

The brief (`execution_brief/chain.rs`, `evidence.rs`, `relevance.rs`,
`decision_log.rs`, `routes.rs`) is built on every prompt-submit event of a
host that has a prompt hook: Claude Code, Codex and Devin through
`task-event`, Gemini through `BeforeAgent`, Antigravity through
`PreInvocation`, and Pi through its package (`extensions/pixel-brief.ts`),
which sends a `brief_only` `task-event` (`start_brief` serves
`TaskProvider::Pi` only for that marker). OpenCode (`plugins/pixel-brief.js`,
V2 `prompt` hook) runs `pixel brief` instead. A prompt starts one
by its shape (`brief_signal`), the typed text only, the last paragraph of a
long untagged paste:

- **code-shaped** (`Signal::Strong`: a backticked name, `snake_case`,
  `camelCase`, a path or a source file): the chain below, no model.
- **weakly code-shaped** (`Weak`: a code word or a code-question opener): the
  same chain, with the intent judge (`pixel classify --if-warm`, only when
  its local server is already listening) free to refuse it (`none`, 0.5 or
  more), and the relevance gate below applied to it too
  (`ENFORCE_GATE_ON_WEAK`). The probes are its retriever, so the concept
  search runs only when they found no file.
- **plain language** (`Prose`: two or more content words, not a request made
  only of `OPS_WORDS` — git, a pull request or an issue, a release, a deploy,
  a steer of the agent's run such as `go next` or `stop` — not an
  acknowledgement): the intent judge, the relevance probe and the meaning
  search run at once on the deadline, and only the gate decides whether the
  repository is the subject. A plural acronym is one keyword (`PRs` is
  `prs`, not the Rust extension `rs`).

A prompt with nothing typed but a `<pasted_content>` block has no shape of its
own, and the paste is someone else's text. It is judged on the head of the
paste's first paragraph and the tail of its last (`pasted_task`: 300 and 299
characters joined by a line break, so the whole stays within `PASTE_TAIL_CHARS`
and neither `brief_task` nor the decision log cuts it; links dropped), and only
ever as plain language, whatever identifiers the paste holds, so the gate above
always decides it. The plain-language rule applies to that excerpt, so a paste
that comes down to operations, one content word or a link stays declined. The
decision log's `typed` is the excerpt, credential shapes masked like any typed
text; a declined paste logs nothing of it.

An acknowledgement, a greeting and a harness envelope start no brief
(`prompt_continuation.rs`). A prompt is an envelope when it opens, after any
leading `<system-reminder>` block, with one of recall's `ORCHESTRATOR_PREFIXES`
(`pixel_recall::intent`): `<task-notification>`, a command wrapper, Codex's
`<realtime_delegation>` and `<send_user_message_question_reply>`. The same list
keeps those turns out of recall's embeddings and session titles, so a new
envelope is added there, once. Codex's IDE extension writes a listing of the
attached files above a `## My request:` heading; only the text under the first
such heading (`user_request`) is the user's, for the continuation check and the
typed text alike.

Unless `PIXEL_BRIEF_FRENCH=0`, a prompt that pixel-mvp's
rule reads as French (`mvp_model::language`: two French function words or
more, outnumbering its English ones) is searched, judged and gated on the
English words code uses (`execution_brief/french.rs`, ADR-0032 of pixel-mvp,
moved under its ADR-0036): each word is dropped when it carries nothing,
replaced by the English words of the imported lexicon
(`crates/mvp-lexicon/fr-en.tsv`), or kept with its accents folded; a
code-shaped token is kept as typed, and a French question word among the
first three becomes its English opener for the gate's `question`. The
signal above, the decision log's `typed` and the probes still read the text
as typed. An English prompt is never rewritten.

The gate (`execution_brief/relevance.rs`, constants in `gate_model.rs`) has
no language model in it. From the daemon's `facts.relevance` (`targets_facts`
with `relevance_only`, which skips the target ranking and is accepted only from
an `algorithm_version` 2 result; in process over the index and graph when no
daemon answers) and the
typed prompt it computes five features: `struct_per_mille` (how many files a
keyword of positive weight names by path or symbol, per thousand indexed
files, as a log), `question` (the prompt ends on `?` or opens with a question
word), `ops_share` (the share of keywords that name a git, release or CI
operation), `struct_ratio` (the heaviest structural co-file's weight over
the keywords' total) and `coverage` (the share of the informative keywords the
heaviest co-file holds, whatever the size of the repository). A fixed logistic model scores them, and two thresholds
cut the score into a tier. The weights are the daemon's
(`pixel_daemon::relevance::row_weight`, `CoFile::weight`), never recomputed.
The model is an L2 logistic regression fitted on the English dev rows of the
brief-gate sets of Pixel (`eval/brief-gate/prompts.jsonl`) and of four repositories
(`prompts-multirepo.jsonl`: pixel, excalidraw, requests, ripgrep; an earlier
model fitted on Pixel alone closed 7 of 13 census answer prompts and 13 of the 19 census prompts the gate sees), thresholds
calibrated out of fold at 5 and 10 % of the off-topic rows
(`scripts/bench/research-gate/results/gate-model.json`, whose SHA-256
`gate_model.rs` cites and a test checks, with `gate_reference.py` as the
executable definition the parity test holds the Rust score to). It does not
apply without a code graph or without a keyword to weigh: such a plain prompt
gets no brief, and a weak one keeps its pre-gate brief when only the graph is
missing.

- **high** tier: the full brief below, with
  `confidence: high — n/m key terms covered; start with the first file`.
- **low** tier: a compact brief, at most three `path:line — text` files within
  700 bytes, no kind route and no search, and
  `confidence: low — possibly related; verify before relying on it`.
- **off**, a probe that cannot answer, or no answer in time: nothing. The
  fused leads of the `meaning` op (daemon only, resident vectors,
  `unavailable` while they build) join the co-files by reciprocal rank in
  the first `files:` of either brief.

When the `meaning` op does not answer (no daemon, no embedding model on disk,
vectors still building), plain-word leads take its leads' place in that fusion
unless `PIXEL_BRIEF_BM25=0|false|off` (`execution_brief/words.rs`, ADR-0030
and ADR-0031 of pixel-mvp, moved under its ADR-0036): BM25 (k1 1.2, b 0.75)
over each graph file's path and its symbols' names and signatures, split and
stemmed by the imported `mvp_model::terms`, a test file at half its score, and
none unless the best file holds two of the prompt's terms. The corpus is read
from the graph on each call, read-only; the leads never reach the gate, the
excerpts or the kind's seed, and a brief whose `meaning` op answered is
unchanged.

The chain runs at most six ops under one deadline, 3 s for a hook (`brief_window_ms` in
`.pixel/config.yaml` or `PIXEL_BRIEF_WINDOW_MS`, clamped to 250 ms to 4.5 s so it ends before the 6 s hook
decision and the host's 10 s kill; 750 ms for the `brief` command, never more than 1 s since OpenCode's plugin cuts it off at 1.2 s). Parts 2 to N
wait for part 1 as long as part 1 may take (the 6 s hook decision bound). A kind-admitted brief whose window ended before its pack
renders the best files as a `confidence: low` lead instead of nothing. The ops are the two probes of
a Weak or Prose prompt, `search-content -F -l` on the first anchor,
`find-symbol` to resolve a uid, `impact <uid>` only
for change or caller intent, `find-code` when nothing found a file, then
one evidence op the question kind picks (`evaluate`/`trace` for a path,
`uses` for covering tests, `skeleton` for a file's shape, `history` for
rationale, `context` for a defect's definition, `targets_facts` for a
feature) plus a bounded `line_at` or context read for the `defined:` line.
A `status` probe (facts freshness, embedding warmth) rides along uncounted.
It reads a warm daemon if one answers this protocol and otherwise the index and
graph read-only in process for that prompt; it never waits for a daemon and
never builds an index or graph. Its history request sends `read_only: true`, which only a
version-15 daemon honours, so the hook can never be the request that opens
the facts warmer. A repository without an index yields no brief. A missing or
stale graph still yields the text evidence (`files:`), with the reason under
`unresolved:` (`the graph is not built`, `the graph is stale`) and no callers.
Output is capped at 2 KiB (`BRIEF_BYTES`) and says how many ops answered
(`coverage: n/m`, `packet partial` when the budget cut short). A confident
(high-tier) brief of a plain-language or weak prompt, whose packet is
complete, is meant to be answered from without a tool call: it opens with a
search receipt (`searched: content+symbols+paths for <probed terms> (<n>
terms, <files> files)`, ` · meaning search returned <n> chunks` only when
that search answered, `ignored (too common)`, and the `result:` line that
tells the agent to verify the matches below) and gathers answer-sized
excerpts (`execution_brief/answer.rs`), shown after the confidence line as
`answer <path>:<line>:` only for the ZERO step below, otherwise used to fix
the ranges to read.
The excerpt is the signature, the first doc-comment line and six lines of the
densest keyword region of a chunk, up to three chunks (`Evidence::lines_at`,
a bounded read outside the op count, refused for a credential-shaped path),
preceded by the routed kind's own block when its data is in: the Flow hops
(or, with one endpoint, the callers) with site and first line, the tests that
mention the target with their first assertion, the config constant line with
its default and where the environment reads it (only lines that carry a
probed term). The generic excerpt is chunk-first: the candidates are the meaning
search's chunks from the five best fused files plus the declaration around
each lexical hit; each is scored by the reciprocal-rank fusion of its own
retriever's rank and the rank of its count of distinct probed keywords; the
best chunk of the file the list ranks first leads, then the best of each
other file; tests, docs and changelogs are
excluded unless the question asks about them, as are `#[cfg(test)]` modules
and test-named symbols. The ranked `files:` list puts tests, docs, `eval/` and changelog files after the code files (a stable demotion, nothing dropped) unless the question asks about them. A plain-language flow or tests question borrows its
symbol anchor from the best non-test chunk. A confident (high-tier) or maybe (low-tier) brief of such a prompt always
ends on one concrete instruction. ZERO: the excerpt is shown, `confidence:
high` is kept and the last line is "Answer from these lines; open a file only
if they don't answer", only when a strict rule holds (`answer::ZERO_RULE`:
the top chunk is the meaning search's best, in the lexical probe's best
co-file, with at least two probed keywords inside). No rule tried on the dev
split of `eval/brief-gate/answer_spans.jsonl` kept span precision at 0.9 with
two firings, so `ZERO_RULE` is `None` and ZERO never fires (`PIXEL_BRIEF_ZERO=0`
would switch it off if one is set). ONE READ, otherwise: the excerpts are
hidden, the confidence is medium, and the tail is `read: <path>:<first>-<last>
— <symbol>` for the best function-level chunk of the first fused file, up to
two `also:` lines for the next two fused files, and "Read these ranges (in
parallel, one turn) before any search. If they don't answer, search as usual." Each
file is cut into function-level chunks (items, merged up to 45 lines), scored
by BM25 over the file's own chunks for the probed keywords (a tie goes to the
meaning search's chunk, which also gets half the best score on top), and
capped at 60 lines around the densest keyword region; tests and docs are
skipped unless asked. A code-shaped prompt that resolved `defined:` reads the
definition's range, then its callers' declarations, then the best chunk of the
top fused files. Its ZERO step (one unambiguous definition of at most 40 lines shown as the answer, `answer::STRONG_ZERO`) is off: 1 of 2 firings was right on dev. The typed text loses its links (`strip_urls`) before the signal is decided, so a pasted `utm_source` is not a code identifier. Ranges come from lines actually read at HEAD, never from a
credential-shaped path.
`scripts/bench/bench-brief-excerpt.py` measures excerpt@1 and `scripts/bench/bench-brief-spans.py` the
ZERO precision and the share of gold spans inside the `read:` ranges. A packet cut by the budget carries neither receipt nor excerpt; one with only an unresolved note keeps them but never the directive. A rationale question gets no excerpt. Config routing needs an
explicit cue (config, setting, env, flag, option, default), not `install` or
`hook`. That
block's cap is `PROSE_BRIEF_BYTES` (3584, under OpenCode's 4000-byte plugin
cap; `PLAIN_PROSE_BRIEF_BYTES`, 2 KiB, without answer evidence); over the
cap the last excerpt block's lines go first, then the block before it, the
first block last, never the receipt. Strong and low-tier briefs carry
neither, nor does one the budget cut or the window ended before it finished.
`PIXEL_BRIEF_RECEIPT=0|false|off` and `PIXEL_BRIEF_ANSWER=0|false|off` remove
the receipt and the excerpts. Every decision, a refusal included, is one line
of `.pixel/brief-decisions.jsonl` (see "On-disk state"), with `receipt`,
`answer` and `excerpt_bytes` for what the block carried.
`PIXEL_BRIEF=0|false|off` or `brief: false` in `.pixel/config.yaml`
switches the brief off.

**The region pack** (`execution_brief/region_pack.rs`, built by `pack_evidence`
after the packet is complete) replaces the packet with the source an agent
reads before it answers, as line-numbered blocks of exact lines under one
header that says what was searched and one directive ("Answer from this
context. Use a tool only if something you need is not shown above."). Its
targets, at most three symbols: the identifiers of the prompt (graph lookup,
else the text search's declarations when the graph is stale, unbuilt or
outdated, marked `graph stale: text fallback`), else the passage it quotes
(searched whole, then by its longest segment), else the names its words spell
in the best files (`rank_identifiers`: at least two of the probed words);
a prompt in plain words also gets the regions `answer_evidence` picked,
and a best file of at most 240 lines is shown whole. Per symbol: the
definition with its doc comment (60 lines at most, a value to its last
line), eight usage sites each in its enclosing function (one per file in
turn, code before tests, same language as the definition), the functions
that use it with their call sites and the tests that reach it through them,
and its tests by name with the first assertion (the first three with their
bodies). A block is cut at a line, never across parts. The detail of each
symbol rises (two lines around a site, a neighbourhood, the whole function,
the neighbours of a constant) while every block still fits. The text may
exceed one hook's 10 000-character cap: `Pack::text` is the whole of it and
`split_parts` cuts it at block boundaries into parts of `PART_CHARS` (9 800)
characters, at most `MAX_PARTS` (8). The count is the `brief_parts`
setting (1 to 8, default 3: the hooks `pixel install` registers), overridden
by `PIXEL_BRIEF_PARTS`; the `brief` command prints at most `CLI_CHARS`
(3 900) unless that variable is set or its caller passes `--parts N`, which
gives it the budget of an N-part hook (pack and probes) as one text. The
OpenCode plugin passes `--parts 3` and accepts up to 30 000 bytes, and Pi's
`brief_only` `task-event` takes the configured `brief_parts` (`brief_budget` in
`task_hook.rs`) and the same 30 000 bytes, so both get what the three Claude and
Codex hooks carry. `PIXEL_BRIEF_PACK=0|false|off`
brings the packet back. A low-tier ("maybe") prompt gets at most one part of it, always as a lead.
A pack of regions alone, with no symbol or passage behind it, ends on
a lead ("not a verified answer") instead of the directive, and one the window
ended before it finished says so. It is built from lines read at HEAD
(`Evidence::lines_at`, refused for a credential-shaped path) and spends none of
the brief's operations. `pack_bytes`, `pack_parts`, `pack_targets`,
`pack_located` and `fallback` are in each decision.

**Locators** (`execution_brief/locator.rs`, asked by `pack_evidence` before the
pack is built). Two questions have no symbol to resolve: where a setting is
defined or defaulted ("what port does the dev server use", "which
environment variables change how it behaves") and which tests cover a topic.
`locator::asks` reads them from the typed words (`config`, `default`, `port`,
`env`, a value word such as `limit` inside a question that opens like one
about a value; `test`, `spec`, `coverage`). The prompt's content words
(`terms`: no setting or test word, no generic one) are stemmed the way
`answer::tokens` reads text, narrowed to the four the relevance probe weighs
heaviest (`pick_terms`: a word it found in too many files is dropped), and
searched with one `Evidence::grep` each for the shapes that define a setting
(a constant, an upper-case assignment, a key given a literal, an environment
read, a flag, a default) and, in parallel, for the upper-case names that carry
a term (`VITE_APP_PORT`), or for the declarations of tests whose name carries
it. A term is matched as a word of an identifier (`port` is in `SERVER_PORT`
and `serverPort`, not in `portal`), and the name a line gives its setting must
carry a term: the words in a value do not make a setting. The best lines
(two for each term first, at most fourteen, four per file; test modules of a
source file, indented lower-case constants, tests and documentation files
dropped) are cut into windows of exact source (a constant with its doc
comment and its whole declaration; a short configuration file whole), a
question about the environment adds the variable names read with the first
read of each (the repository's own prefix first), and a question about tests
adds the matching tests by name with `path:line`, the first lines of the best
eight and, when a manifest vouches for a runner (`pytest`, `vitest`, `jest`,
`cargo test`, `go test`), the command that runs only them. The blocks join the
pack after the symbols' own and ahead of nothing but the graph facts; a block
over lines a definition already shows is dropped, and a pack of locator blocks
alone ends on the directive. Quoted tokens that read as credentials are
masked, and a credential-shaped path never reaches the searches' rows
(`.env*` files stay out of the brief). A locator spends no brief operation,
lives inside its own 300 ms (`LOCATOR_WINDOW`) and the brief's window, and a
search that fails leaves the pack as it was.

**The session's files** (`execution_brief/session_paths.rs`, started by the
hook beside the worker and given `CONTEXT_WINDOW`, 150 ms of the brief's
750). A prompt of a hook session ("now fix the test") often says too little to
place in the repository, so the brief also reads where the session already is:
the files its last tool calls read or edited, found in the structured inputs
(`file_path`, `path`, a shell `command`) of the last 64 KB of the
host's `transcript_path` (Claude Code and Codex payloads), newest first, and
the dirty files of `git status`, most recently written first. Only a path leaves
the module: a candidate counts when it exists inside the repository once its
links are resolved and is not credential-shaped, so transcript text reaches
neither the brief nor a log. The two lists rank as one list that joins the
fusion of the meaning leads and the relevance co-files as a third, boosting only: a file
those lists already name gains its reciprocal rank, and a file only the session
names does not enter, so an unrelated dirty file never displaces a lead. A
short follow-up (`is_follow_up`: at most ten words that ask for a change or
lean on what came before) that the gate would have closed is let through
instead, when the session has files: the files are the brief
(`Admission::Context`, gate `open`, reason `context: …`, rendered as a low-tier
maybe) and the off-topic leads of its words are dropped. The lift is checked
before the relevance probe's outcome (`refusal_lifting`), so a probe that
failed (`relevance: facts: index_stale`) or did not answer in time no more
closes the follow-up than an off-topic score does, and a lifted brief counts as
answered when neither probe could be asked: the session's files alone make it.
A source that has not answered inside its window adds nothing.
`PIXEL_BRIEF_CONTEXT=0|false|off` turns it off, and the `brief` command, which
has no session, never uses it.

**Evidence sent once** (`execution_brief/sent.rs`). A session that asks again
about code it was just sent would get the same lines again, and they would fill
the pack's room that source it has never seen could use. So the pack of a hook
prompt remembers its blocks' ranges per `session_id` (`.pixel/brief-sessions/`,
see "On-disk state") and the next one leaves out any block that overlaps one:
the definition, the usage windows, the callers' windows, the test bodies and the
locators' blocks. The room they would have taken goes to the sites after them
(`usage_candidates` weighs twice the sites when some were sent, and shows the
first eight that are new), and a pack that is left with no source falls back to
the packet's lines. Only the pack's ranges are remembered; the `brief` command
has no session and sends everything. The record is forgotten after 24 hours and
when the host starts the session over: `SessionStart` with source `compact` or
`clear`, which is when the agent stops holding what it was sent. Codex raises no
`SessionStart` for a compaction, and `PreCompact` is not among the hooks
`pixel setup` registers (registering it would have every Codex user review the
hooks again), so a Codex compaction is covered by the 24 hours alone.
`PIXEL_BRIEF_DEDUPE=0|false|off` turns it off.

**Question kinds without a symbol** (`execution_brief/kinds.rs`, asked by
`pack_evidence` beside the locators). `routes::QuestionKind` gained `Overview`
(a tour: "quick tour", "lay of the land", "what each crate is for"; a
cue of how the whole is put together needs a noun for the whole) and `Review`
(a review cue beside a change noun: "review the latest commit", "is this PR
safe to merge"), read from the words, with no model, and ahead of a judge's
verdict; the walkthrough phrases ("walk me through", "what happens when",
"end to end") route to `Flow` with or without a symbol. The relevance gate
closes these prompts because they are not phrased in the repository's words,
so `admitted_by_kind` lets a tour and a review through (`Admission::Kind`, logged
as gate `open` with reason `kind: overview`) and a walkthrough only when it
shares two words with a file the graph knows by its symbols. Their packs are
the evidence itself, built from three reads added to `Evidence`:
`repo_map` (the indexed files, their top-level declarations and `main`
functions, read from the published graph in `graph_reads.rs`: areas are
directories split while an area holds more than 60 files, an area is shown by
its four best declarations as `path:line kind name`, tests, documentation and
scripts are one line, and a repository of 60 files or fewer is listed file by
file), `changes` (one `pixel_git::GitRunner` call bounded by what is left of
the window, `git show`, `git diff HEAD` or the diff against the merge base,
cut into files, line counts and the new-side range of each hunk in
`git_changes.rs`) and `flow_tree` with `find_symbols` (the calls out of a
function in the order the source makes them, from `edges`). An overview
carries the map, the first 70 lines of `ARCHITECTURE.md` and its outline, the
head of the manifest and of the README; a review the change summary (counts,
and the source files no changed test file is about), the declaration around
each hunk (code before tests, at most ten regions of sixty lines), the
touched declarations and the callers of the functions among them; a walkthrough
the call trace and the body of each call, from the symbol the prompt names,
the call path the chain found, or the function whose name carries the rare
words of the prompt and from which most of the others are reached
(`entry_score`). The regions closest to a few words are left out of the pack of
a tour and of a review. A question about history in so many words ("which commit", "when did",
"who added", `blame`) is `QuestionKind::History`: it is admitted like a
walkthrough, and its pack is the commits of the history index
(`build-index --history`, read through the daemon's read-only `history` op:
no ingest, no plugin) that match the prompt's content words, as `sha date
subject`, what the two best commits changed (`path:line +a -r`, one bounded git
call each) and the paths and changed lines the search matched. An absent
history index leaves the prompt as it was. A TS/JS class property holding an
arrow function is a method of its class in the graph (extractor 25), so a
walkthrough can start at a handler written that way.
The reads add no brief operation and fail open.

**Keeping a daemon behind the brief.** Without a daemon the brief has no
`meaning` leads, and a daemon exits after thirty minutes idle or becomes
unusable when the protocol moves, so the hooks bring one back
(`execution_brief/autostart.rs`). Both moments decide the same way: the
brief is on, the repository has an index, and auto-start is on (the CLI's own
switch, `PIXEL_DAEMON_AUTO_START=0` or `daemon_auto_start: false`); then
`probe_daemon` says what answers. Nothing answers: launch. An older protocol:
send it Shutdown, then launch. A newer protocol: leave it to that pixel. A
current daemon: nothing to launch for a prompt, but a `SessionStart` still
launches, because the start process is what asks the daemon its first
question and wakes the `meaning` vectors. The launch is one detached `pixel
daemon start <root> --warm-meaning` (`spawn_detached`, its own process
group, no standard streams), which retires a stale daemon and waits for it to
exit, starts the foreground daemon through `spawn_daemon`, waits for it to
answer and asks the one question. A prompt that finds no daemon still briefs
from the index in process; the daemon is for the next one. The hook decides on
a thread of its own and waits `HOOK_BOUND` (100 ms) for the decision at most,
so a wedged socket costs a start, never the hook; the decision log records
what came of it (`daemon`: `launched`, `running`, `skipped: …`,
`launch failed`) beside where the evidence came from (`route`: `daemon` or
`local`). The `daemon start --warm-meaning` process is not metered as a user
command.

`pixel doctor` checks current installation artifacts and distinguishes configured
or protocol-checked hooks from observed live execution.
Every check is listed in `pixel_setup::doctor::CHECKS` with a stable id and
the command that repairs it (`pixel doctor --list`): `--only`/`--skip` select
by id or by group (`install.*`), and each yellow or red check reports that command as `fix`.
`install.claude-hooks` is also yellow when complete hooks run another binary than
the managed one doctoring them (a global `pixel-dev setup`, a previous release's
path); a `pixel-dev` doctor never judges the home install.
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

Search path failures include the effective process cwd in their diagnostic
and action-log error. If a caller launches Pixel outside its intended repo,
the failure record belongs to the root discovered from that effective cwd;
an absent record in the intended repo does not establish that no call ran.
The shell's `PWD` variable does not override the process working directory.

The byte approximation is roughly one token per four UTF-8 bytes and includes
reporting overhead. Measured output covers rendered CLI stdout, CLI-owned diagnostics and top-level
errors, not lower-level library or subprocess streams. V1's fallback volume policies are 4 KiB per assumed distinct
returned file read and 1 KiB per native-command output. They are assumptions, not
measured averages. `workflow-v2` measures the one case it can: `list-signatures`
stands in for reading one whole file, so its baseline is that file's size (a
`stat`, no source read) with no assumed command, and the live line states
`full read N tok, pixel answer M tok (-X%)` from the file and the stdout answer
(`answer_bytes`), both floored bytes / 4 as in `scripts/bench/bench-read-savings.sh`.
Records keep the version they were written with. Estimates consider only returned evidence/relationships and
represented native steps. Partial results remain partial; meaningless comparisons
are unavailable; zero and negative savings are retained. There is no external
telemetry, hidden reasoning estimate, or monetary claim.

Time accounting is separate from byte accounting. An optional `time_estimate`
records `estimator_version: sequential-v1`, `round_trip_ms`, `native_command_ms: 0`,
`sequential_steps`, and signed `saved_ms`. Legacy records lacking these fields
remain unavailable for time comparisons; they are not silently recalculated.
The `token-savings` plugin's report adds `time_estimates` groups keyed by token/time estimator
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
record is unsafe under concurrency and must never be used. Claude Code, Codex and
Antigravity use host-supported metrics hooks where available; mock-wrapper tests
prove command output and stream/exit preservation, not actual model adherence or
live-host duplicate suppression. Chat relay remains a separately verifiable
boundary.

Codex's metrics hook requires the finalized record's exact identity header
(command, duration and short invocation ID) as a complete string line in the
current `tool_response`, together with the command-argument and effective-cwd
match. It rejects legacy records without an ID, colliding headers and results
with several matching footers. Without that evidence it emits nothing: an
earlier same-command record cannot establish that this call ran. Other hosts
retain their command/cwd lookup and overlapping-invocation suppression.


### Brief probes

`probes/<name>/` holds scripts that add blocks to the prompt-submit brief (contract: `CONTRIBUTING.md`, "Write a
probe"). A probe is a plugin of the host (`pixel-plugin`): a `pixel-plugin.toml` with `capabilities =
["brief-probe"]` and a `[brief-probe]` table (`owner`, `when_kinds` / `skip_kinds` / `when_prompt`, `budget_ms` at
most 2500, `mode` lead or append, and `when_gate_closed`) beside an executable named by `run`. `pixel_plugin::probes` lists the probes that
may run: the user tier (`~/.pixel/plugins/<name>/`, first-party and the user's own) as installed, the repo tier
(`<repo>/.pixel/plugins/<name>/`) only at the digest `pixel plugin trust` recorded, minus the ones
`pixel plugin disable` switched off (`disabled` in `~/.pixel/plugins.toml`; `enable` undoes it).

`pixel setup` writes fourteen curated first-party probes (embedded from the explicit allowlist in
`pixel-setup::bundled_probes`, each
directory marked so setup updates only its own copies and `--remove` deletes only them) next to the Claude or Codex
brief hook; `PIXEL_BRIEF_PROBES=0` runs none. In `execution_brief::chain::Pending::finish_with_record`, once `fit`
has rendered the brief, `execution_brief::probes::apply` starts every probe whose
`when` fits the question in parallel, each with `{prompt, repo_root, brief, kind, session_files}` as JSON on
stdin, in an emptied environment (`PATH`, `HOME`, `LANG`, `PIXEL_BIN`), inside `min(budget_ms, what is left of the
window)` (the chain leaves them at least 1.2 s even when it used the window up, and none outlives 5 s after the
prompt). A probe that overruns is killed; one that fails, prints nothing or is not due is skipped: fail open.
An unjudged prompt can use matching probes. A relevance-closed gate permits only probes declaring
`when_gate_closed = true`, which also requires an explicit nonempty `when_prompt` matcher; an intent denial,
explicit silence, disabled briefs, and unindexed repositories remain off. Recovery can supply an otherwise empty
brief without changing its logged relevance verdict. The shipped set includes caller/test evidence, the bodies of the
test cases a tests question asks about, complete flow bodies, stop-condition evidence, setting application sites, commit patches, and Cargo-member overviews.
Experimental directories are not automatically shipped; the installer tests assert the exact allowlist.
`probes::merge` folds what they print into the brief: `drop: <regex>` removes the brief's blocks whose header
matches, `first: <regex>` moves them to the front, lead blocks go before the brief's and append blocks after, one
block per header, the whole within the hook parts the host takes (3 x 9,800 characters) with append blocks shed
first and the closing directive kept last. The merge is pinned by the golden cases of
`crates/pixel/tests/fixtures/probe-merge.json`, which `scripts/bench/probe-test.py` (the specification and the harness)
and the Rust test both run. Each run (name, milliseconds, bytes, outcome) is a `probes` entry of the prompt's line in
`.pixel/brief-decisions.jsonl`. `scripts/bench/probe-equivalence.py` checks the native brief against the reference chain
`scripts/bench/probe-chain.py` on the census questions.

## Testing and gates

- `just ultima-arena` (or `scripts/dev/ultima-arena.sh`) runs the five-CLI
  brief/metrics smoke in the current Herdr operator tab. A fixed seed controls
  start order; all ready panes receive the same prompt before monitoring starts.
  Each turn is interrupted once its brief and matching invocation metrics are
  evidenced. One process lock protects the arena grid. Dated receipts, session
  evidence and timestamped logs stay under gitignored `eval/arena-results/`;
  its Markdown index records run numbers, repository commits and dirty state.
  `--self-test` exercises the orchestration and verdicts with fixture CLIs.
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
  Its pull-request path filter also includes `docs/bench/**` so benchmark
  reports receive the required status checks.
  - **Test + Format**: action-pin verification (`scripts/ci/verify-action-pins.py`),
    the SPDX header check (`scripts/ci/check-spdx.py`, on every diff),
    `cargo fmt --check`, `cargo nextest run --profile ci`
    (`.config/nextest.toml`: one process per test, retry once but fail on
    flaky, kill after 180 s), `cargo test --doc`, a check that the tests left
    the checkout's `.pixel/actions.jsonl` alone, the `scripts/test-*.py`
    contract scripts (installer, gate runner, pre-push no-op,
    release prepare, Homebrew formula and Linux
    bottles, release SBOM, homebrew-core formula,
    nightly diff checkpoints, coverage selection, mutants
    config, action pins, advisory ignores, SPDX headers, clean, cancel-stale sweep, embedding-model pin, release symbols split, public-claims check, harness-grid dispatch input,
    reproducible release build environment, the brief-gate bench, excavation counter and live brief A/B driver self-tests with the `eval/brief-gate/` prompt set's schema check, the `eval/` agent A/B harness against fixture CLIs), the
    pixel-retro lead-time and adherence contracts
    (`.agents/skills/pixel-retro/test_lead_time.py`, `test_adherence.py`);
  - **Lint**: `cargo clippy --all-targets` with warnings denied, then
    `cargo check` of the two reduced feature lanes (`--no-default-features`,
    `model2vec` only);
  - **MSRV**, **Ranking gates** (the NDCG@10 relevance bench and the
    `m1_latency` service-time bench, both in `--test` mode, so a red lane
    fails the job; the NDCG lane embeds with potion-code-16M-v2 pinned to one
    Hugging Face revision by `scripts/ci/fetch-embedding-model.sh`, restored
    from an actions/cache entry keyed by that revision, and runs behind a
    closed proxy so it never reaches the hub) and **Dependency policy** (`cargo deny`, then
    `scripts/ci/check-advisory-ignores.py`: `osv-scanner.toml`, which Scorecard
    reads, accepts the same advisories as `deny.toml`).
- `mvp-reference.yml` runs the imported index truth test on the pinned public
  once-campfire and Pixel repositories when its sources change. The ordinary
  pixel-graph suite also exercises corruption recovery, incremental/full
  equivalence and provenance through the production refresh adapter.
- Other workflows: `mutants.yml` (01:17 UTC on `main`, only the cumulative
  diff since the latest completed campaign; no pull-request trigger).
  `scripts/ci/mutants-nightly-range.py` selects a checkpoint from trusted
  completed main-run artifact metadata and refuses API/history failures.
  A checkpoint is uploaded after every listed mutant has a recognized
  outcome; survivors keep the run red, while missing outcomes and disk-full
  failures leave the prior checkpoint. Unchanged main starts no Rust jobs.
  The report labels listing-only results `planned`, complete recognized
  outcomes `tested`, and missing/invalid outcomes `incomplete`; it reports
  the listed count separately from the outcomes recorded across shards.
  `.cargo/mutants.toml` leaves out benches and crate-root build scripts;
  the owned `mvp-*` libraries participate. `scripts/test-mutants-config.py`
  refuses any other exclusion. The existing plan/shards/report share that file and distribute
  shards over `PIXEL_MUTANTS_SHARD_RUNNERS` (GitHub-hosted `ubuntu-26.04`
  by default). Only scheduled CI executes mutations; there is no manual,
  local or pull-request campaign. The pre-push hook is a no-op; local validation is optional.
  Other lanes include
  `cross-build.yml` (the three release lanes),
  `perf.yml` (Mondays at 04:23 UTC and on demand: `pixel prepare-repo
  --json` on the pinned `excalidraw/excalidraw` corpus from nothing, then
  after one appended line, each receipt judged by `scripts/ci/check-perf.py`
  against `.github/perf-baseline.json` at its `ceiling_multiplier`; it also
  fails a receipt that measured nothing, such as a reused index on the cold
  run or a graph under its file and symbol floors, and uploads both receipts
  as `perf-receipts`; a second job, `brief-latency`, runs
  `scripts/bench/tool-census/score_brief.py` on the 56 questions of
  `eval/brief-bench/` through the prompt-submit hook route, with this
  commit's probes frozen by `freeze-home` and the pinned embedding model,
  and `check-perf.py --label brief` judges the report's p95 and max
  `elapsed_ms` and its `partial` count, refusing a report that timed fewer
  questions than the floor, ran installed probes or another question file;
  a third job, `tree-delta`, runs `cargo bench --bench tree_delta` on the
  same corpus and a generated 10 000-file tree with `PIXEL_BENCH_JSON`, and
  `check-perf.py --label tree_delta` judges each subject's `tree_delta` p50,
  its file floor and its sample count. A tripwire, not a benchmark),
  `reproducible-build.yml` (the `x86_64-unknown-linux-musl` release binary
  built twice from two checkouts at different paths, no cache, failing
  unless the two sha256 match; on pull requests touching the build
  environment, `Cargo.lock`, a `Cargo.toml`, `crates/pixel/build.rs` or
  `release-build.yml`, on pushes to `main` touching any of them or
  `crates/**`, weekly and on demand), `release.yml`
  (on a tag: `verify`, then publication, the tap and the post-publish smoke
  test) and `release-build.yml`, the reusable workflow it calls to build the
  archives, write the formula and the Linux bottles with
  `scripts/release/homebrew-formula.py` and the homebrew-core formula with
  `scripts/release/homebrew-core-formula.py`, write each archive's CycloneDX SBOM
  `pixel-<tag>-<target>.cdx.json` (cargo-cyclonedx, narrowed by
  `scripts/release/release-sbom.py` to the crates `cargo tree -p pixel-cli` compiles
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
  `scripts/ci/coverage-nightly.py` skips both expensive jobs when the latest
  successful scheduled run already measured this SHA; failed or cancelled
  measurements are retried. Read-only Actions access supplies run metadata.
  Reports contain line, region and function totals and one row per crate in the job
  summary, the report as the `coverage-summary` artifact; it fails on a red
  test or on line coverage under 90%, the OpenSSF gold bar; its `branches`
  job runs the same suite on a dated nightly under `cargo llvm-cov
  --branch` and writes branch and line totals, one row per crate, to its
  summary; it uploads the raw JSON report as the `coverage-branch-summary`
  artifact and the per-line lcov report as `coverage-branch-lcov`, which
  `scripts/ci/coverage-uncovered-branches.py` turns into uncovered branches per
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
Git contract runs in both `scripts/ci/gates.sh` and CI
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

Release builds are reproducible. `scripts/release/release-build-env.sh` sets
`SOURCE_DATE_EPOCH` to the commit's time, `RUSTFLAGS` to
`--remap-path-prefix` the checkout (`/pixel`) and `CARGO_HOME` (`/cargo`),
and `CARGO_PROFILE_RELEASE_STRIP=none`: `[profile.release]` strips symbols
for every other build, while a release keeps them in its one build and
`scripts/release/split-symbols.sh` archives that binary as
`pixel-<tag>-<target>.symbols.tar.xz` before stripping the shipped copy
(Apple `strip` on macOS, llvm-strip from `llvm-tools` on Linux; every
`cross-build.yml` lane runs the same split);
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
homebrew-core formula (`scripts/release/homebrew-core-formula.py`) builds with it and
with `--no-default-features --features model2vec`, since homebrew-core builds
from source, owns updates, and refuses the prebuilt ONNX Runtime `fastembed`
downloads; every other build keeps the check.
