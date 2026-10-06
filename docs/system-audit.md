# System audit ledger — 2026-09-12

## Scope and evidence rules

Inventory: **113 Clap leaf commands; 116 effective leaves** after expanding the positional `note` dispatcher into `set`, `get`, `rm`, and `list`. `help` and aliases are not independent behaviors. Includes **16 Cargo workspace packages**. Inventory was obtained by recursively executing installed CLI `--help`; this verifies discoverability only, **not functional correctness**. The candidate must be compared against this inventory after rebuilding.

**116/116 effective leaves have executable boundary observations**, not merely help coverage. A1 covers 87 unique leaves with 102 passing assertions; A2 adds recall/install/daemon boundaries with 28 passing checks; F2 covers seven flow leaves; A3 covers repository ask. Two leaves (`recall setup`, `recall embed`) have **refusal-only** observations, not successful model download/embedding. PASS means only the stated fixture/assertion, not every option or advertised guarantee. Relevance and final release gates remain separate blockers.

Safety: fault injection uses disposable homes, repositories, local bare remotes and fake subprocesses only. No personal configuration, authenticated browser, live remote or host daemon is a failure fixture.

## Observed regression evidence

| ID | Executable check | Observed outcome |
|---|---|---|
| F1 | `cargo test -p pixel-flow replay_shell_preserves_arguments_and_never_executes_data` | RED before fix: generated quoted URL broke shell parsing; after escaping and comment hardening, GREEN in 36-test crate run. |
| F2 | `cargo test -p pixel-cli --test flow_cli` | RED: 0 passed / 3 failed before CLI dispatch fixes. Dry-run executed fake browser; JSON replay printed prose; browser exit 7 returned CLI exit 0. Candidate rerun GREEN: 3 passed, 0 failed, exit 0. |
| F3 | `cargo test -p pixel-flow` | GREEN: 36 passed, 0 failed (includes real `/bin/sh` with fake agent-browser, no actual browser). |

## Fixture and executable-test families

| Family | Fixture/contract boundary | Existing executable command (not leaf-proof) |
|---|---|---|
| R | Tracked disposable repository with identifiers, ignored files, deletion, rename, overlay and symlink controls | `cargo test -p pixel-index -p pixel-graph -p pixel-context -p pixel-rank -p pixel-daemon` |
| H | Disposable multi-commit repository including deleted and renamed symbols | `cargo test -p pixel-facts` |
| G | Disposable worktrees and local bare remotes; divergent tips and interruption checkpoints | `cargo test -p pixel-git -p pixel-ops` |
| E | Disposable .env with comments, duplicate keys, unrelated secrets and snapshots | `cargo test -p pixel-ops --test envfile` |
| I | Disposable HOME, existing third-party hooks/config and fake pixel binary | `cargo test -p pixel-install` |
| C | Disposable transcript sources and corpus; malformed records and unavailable model | `cargo test -p pixel-recall` |
| S | Disposable error SQLite store; error/event/run JSON, fake failing subprocess and MCP requests | `cargo test -p pixel-session` |
| F | Disposable HOME/flow store and fake agent-browser; real shell executes generated argument data | `cargo test -p pixel-flow; cargo test -p pixel-cli --test flow_cli` |
| T | Disposable task ledger/worktrees; fake worker processes and malformed provider payloads | `cargo test -p pixel-cli --bin pixel` |
| A | Disposable action log with measured, partial, unavailable and malformed accounting rows | `cargo test -p pixel-actionlog; cargo test -p pixel-cli --test metrics_cli` |

## CLI leaf ledger

The contract text below is inventory evidence, not an endorsement of wording; notably `ask` help historically described cosine ordering even when RRF determined ranking.

| Leaf | Advertised contract | Fixture/test family | Observed leaf outcome |
|---|---|---|
| `ask` | Semantic code search: embed a natural-language question ("how is authentication handled?") and rank files by weighted semantic/lexical RRF (cosine score retained separately) | A3 | A3 PASS required manual ranks 3/2; broader semantic-imports evaluation FAIL |
| `branch` | Create a new branch from HEAD (or --from <ref>) | A1 | A1 PASS: contains `audit`; valid JSON |
| `branches` | One-call read-only branch inventory: ahead/behind, merged, stale, unpushed — the deterministic "did you push everything?" answer | A1 | A1 PASS: contains `main`; valid JSON |
| `changes` | Symbols/flows affected by working-tree changes | A1 | A1 PASS: contains `lib.rs`; valid JSON |
| `clusters` | Functional-area clusters | A1 | A1 PASS: valid JSON |
| `context` | Budget-fitted context for a symbol uid | A1 | A1 PASS: contains `login_user`; valid JSON |
| `daemon start` | Start the daemon (background unless --foreground) | A2 | A2 PASS populated/installed fixture; exact argv/exit in boundary observations |
| `daemon status` | Check whether a daemon is running | A2 | A2 PASS populated/installed fixture; exact argv/exit in boundary observations |
| `daemon stop` | Stop a running daemon | A1 | A1 PASS: exit 0; fixture state checked |
| `diff` | Structured diff between two refs (or ref → working tree) | A1 | A1 PASS: contains `changed_function`; valid JSON |
| `doctor` | Health check: install state, daemon, index/graph/facts freshness | A2 | A2 PASS reporting: 6 green / 2 red / 3 yellow, ok=false; not all-green health |
| `env check` | Verify required keys exist (names only) | A1 | A1 PASS: valid JSON |
| `env inventory` | List .env files under root — key NAMES only, never values | A1 | A1 PASS: valid JSON |
| `env restore` | Restore from a snapshot (latest if --snapshot omitted; undoable) | A1 | A1 PASS: valid JSON |
| `env set` | Set one key (snapshot-first; every other line byte-preserved) | A1 | A1 PASS: valid JSON |
| `env snapshots` | List snapshots recorded for a file | A1 | A1 PASS: valid JSON |
| `excavate` | Engine 2: history-wide discovery (rescue v2) | A1 | A1 PASS: contains `removed_manual_history`; valid JSON |
| `flow delete` | Delete a flow by name | F | F2 PASS saved-flow lifecycle/JSON; replay success, failure and conflicting flags |
| `flow get` | Retrieve a flow by name (for the agent to follow deterministically) | F | F2 PASS saved-flow lifecycle/JSON; replay success, failure and conflicting flags |
| `flow list` | List all saved flows, optionally filtered by tag | F | F2 PASS saved-flow lifecycle/JSON; replay success, failure and conflicting flags |
| `flow replay` | Emit ready-to-run agent-browser commands with variable substitution | F | F2 PASS saved-flow lifecycle/JSON; replay success, failure and conflicting flags |
| `flow revise` | Update an existing flow's metadata and/or steps | F | F2 PASS saved-flow lifecycle/JSON; replay success, failure and conflicting flags |
| `flow save` | Create a new flow | F | F2 PASS saved-flow lifecycle/JSON; replay success, failure and conflicting flags |
| `flow show` | Pretty-print the full flow document (human-readable) | F | F2 PASS saved-flow lifecycle/JSON; replay success, failure and conflicting flags |
| `graph` | Force (re)build of the code graph db | A1 | A1 PASS: contains `symbols`; valid JSON |
| `history` | Commit history with detail levels and byte caps | A1 | A1 PASS: contains `fixture`; valid JSON |
| `history-search` | M3: history-wide fact + diff search | A1 | A1 PASS: contains `removed_manual_history`; valid JSON |
| `hook guard`, `composed-guard`, `session-start`, `prompt-submit`, `post-compaction`, `post-tool-use`, `metrics` | Retired verbs, kept as silent no-ops so an old hook registration cannot fail a host | A1 | `scripts/system_audit.py` asserts exit 0 and empty stdout for each; the 2026-09-12 run predates the retirement |
| `impact` | Blast radius of a symbol (callers upstream / callees downstream) | A1 | A1 PASS: valid JSON |
| `index` | Build (or rebuild) the text index for a directory tree | A1 | A1 PASS: exit 0; fixture state checked |
| `inspect` | Show repo state: HEAD, branch, dirty files, fingerprints | A1 | A1 PASS: contains `lib.rs`; valid JSON |
| `install` | Idempotently deploy the agent prompt and Claude/Codex shell wrappers | A2 | A2 PASS populated/installed fixture; exact argv/exit in boundary observations |
| `journal` | M5: journal a session event (fire-and-forget) | A1 | A1 PASS: valid JSON |
| `lifecycle` | Engine 2: lifecycle of a path or token | A1 | A1 PASS: contains `removed.md`; valid JSON |
| `log` | Self-assessment: pixel's own action log (what ran, what went wrong) | A1 | A1 PASS: valid NDJSON |
| `map` | Structural repo map: every indexed file with its symbols | A1 | A1 PASS: contains `login_user`; valid JSON |
| `migrate` | Prepare .pixel/ state directory and remove legacy .gitpixel/; does not claim rebuilt indexes | A2 | A2 PASS populated/installed fixture; exact argv/exit in boundary observations |
| `note get` | Read the annotation for a file and target. | A1 | A1 PASS: valid JSON |
| `note list` | List stored annotations within optional file scope. | A1 | A1 PASS: valid JSON |
| `note rm` | Remove only the selected annotation. | A1 | A1 PASS: valid JSON |
| `note set` | Persist one annotation without rewriting source. | A1 | A1 PASS: valid JSON |
| `processes` | Discovered execution flows | A1 | A1 PASS: valid JSON |
| `provenance` | Per-region blame attribution: who introduced/owns each region of a file | A1 | A1 PASS: contains `fixture`; valid JSON |
| `publish` | Stage files, commit, and optionally push (crash-safe, idempotent) | A1 | A1 PASS: valid JSON; empty `--files` scope stages the complete working tree |
| `push` | Leased push to a remote (crash-safe, idempotent) | A1 | A1 PASS: valid JSON |
| `query` | Compile and execute one bounded deterministic retrieval recipe | A1 | A1 PASS: contains `login_user`; valid JSON |
| `ready` | Make a repository ready for agent work: index, graph, and warm daemon | A1 | A1 PASS: valid JSON |
| `recall ask` | Natural-language hybrid search (lexical + semantic), grouped by session | A2 | A2 PASS populated corpus, lexical-only; semantic model execution unverified |
| `recall context` | Token-budgeted context pack for a query — headers, snippets, then full turns, greedily fitted for LLM consumption | A2 | A2 PASS populated corpus, lexical-only; semantic model execution unverified |
| `recall daemon start` | Start the recall daemon (background unless --foreground) | A2 | A2 PASS populated/installed fixture; exact argv/exit in boundary observations |
| `recall daemon status` | Check whether the recall daemon is running | A2 | A2 PASS populated/installed fixture; exact argv/exit in boundary observations |
| `recall daemon stop` | Stop the running recall daemon | A2 | A2 PASS populated/installed fixture; exact argv/exit in boundary observations |
| `recall embed` | Embed pending turns into the semantic index (resumable) | A2 | A2 REFUSAL PASS: invalid local model exits 1; successful model path unverified |
| `recall export` | Bulk-export ingested sessions, one file per session, into a folder | A2 | A2 PASS populated/installed fixture; exact argv/exit in boundary observations |
| `recall index` | Ingest transcript sources into the corpus (incremental by default) | A2 | A2 PASS populated/installed fixture; exact argv/exit in boundary observations |
| `recall maxtest` | MAX TEST: rank remembered keywords by rarity — the term with the fewest matches pins the session you're hunting for fastest | A2 | A2 PASS populated/installed fixture; exact argv/exit in boundary observations |
| `recall search` | Regex search over every indexed transcript turn, newest first | A2 | A2 PASS populated/installed fixture; exact argv/exit in boundary observations |
| `recall sessions` | List indexed sessions, newest first | A2 | A2 PASS populated/installed fixture; exact argv/exit in boundary observations |
| `recall setup` | Download and verify the embedding model (one-time) | A2 | A2 REFUSAL PASS: invalid local model exits 1; successful model path unverified |
| `recall show` | Print a session's turns | A2 | A2 PASS populated/installed fixture; exact argv/exit in boundary observations |
| `recall status` | Corpus freshness, counts, and storage location | A2 | A2 PASS populated/installed fixture; exact argv/exit in boundary observations |
| `reconcile` | Engine 4: one-call deterministic branch sync | A1 | A1 PASS: valid JSON |
| `rescue` | Surgical revert planner: locate the files a problem points at, list recent versions with the likely-breaking commit flagged, recommend a last-known-good candidate | A1 | A1 PASS: contains `lib.rs`; valid JSON |
| `resolve` | Engine 1: resolve a phrase to code via the concept index | A1 | A1 PASS: contains `login_user`; valid JSON |
| `review` | Review working-tree changes (staged, unstaged, untracked, conflicted) | A1 | A1 PASS: contains `lib.rs`; valid JSON |
| `rewrite` | Squash every commit on the current branch since its base into ONE commit (crash-safe, backup-ref'd), optionally force-pushing with lease | A1 | A1 PASS: valid JSON |
| `savings` | Token-savings report: for retrieval-shaped commands (search/query/ context/resolve) that recorded snippet-vs-pool volumes, aggregate the fraction of the candidate pool… | A1 | A1 PASS: valid JSON |
| `search` | Search the indexed tree with a regex pattern | A1 | A1 PASS: contains `login_user` |
| `search-compat` | Native-output literal file search for automatic routing; unsupported inputs execute the original rg/grep command without modification | A1 | A1 PASS: contains `login_user` |
| `ship` | Publish + push in one op (commit then leased push) | A1 | A1 PASS: valid JSON |
| `skeleton` | All signatures in a file — the skeleton view at ~10% of Read cost | A1 | A1 PASS: contains `login_user`; valid JSON |
| `sniper cursor` | Print the current cursor (highest error id) | A1 | A1 PASS: contains `1`; valid JSON |
| `sniper env` | Latest run fingerprint; --diff compares against the previous run | A1 | A1 PASS: contains `audit-run`; valid JSON |
| `sniper gc` | Apply retention now; --vacuum compacts the database file | A1 | A1 PASS: valid JSON |
| `sniper hmr` | "Was my edit applied?" — recent HMR/reload/dep-optimize events | A1 | A1 PASS: contains `lib.rs`; valid JSON |
| `sniper last` | Newest errors, compact one-liners + `cursor:` footer | A1 | A1 PASS: contains `audit_error`; valid JSON |
| `sniper mcp` | Run the stdio MCP server (tools: errors_since, error_show, errors_query, hmr_status, env_fingerprint) | A1 | A1 PASS: initialize, list exactly 5 tools, call all 5 against populated store, EOF cleanup |
| `sniper query` | Substring search over stored errors | A1 | A1 PASS: contains `audit_error`; valid JSON |
| `sniper report` | Ingest one JSON record ("-" = stdin) | A1 | A1 PASS: valid JSON |
| `sniper run` | Wrap a command: tee its output live, mirror its exit code, and on failure record structured errors (tsc parsed per TS code; otherwise a generic tail record + full outp… | A1 | A1 PASS: exit 9; fixture state checked |
| `sniper show` | Full detail for one error id: frames, values, run fingerprint, ±30s events | A1 | A1 PASS: contains `audit_error`; valid JSON |
| `sniper since` | Errors newer than a cursor (footer of every listing), or --ts 5m | A1 | A1 PASS: contains `audit_error`; valid JSON |
| `sniper test` | Latest test signal (vitest failure record vs test-pass event) | A1 | A1 PASS: contains `test-pass`; valid JSON |
| `stats` | Show raw shard metadata (legacy) | A1 | A1 PASS: contains `files` |
| `status` | Index + graph freshness status | A1 | A1 PASS: valid JSON |
| `symbol` | Look up symbols by name in the code graph | A1 | A1 PASS: contains `login_user`; valid JSON |
| `sync` | Fetch from a remote (idempotent) | A1 | A1 PASS: valid JSON |
| `targets` | Sniper target list: task description in, closed prioritized file list out (P0 = start here, P1 = likely, P2 = droppable) | A1 | A1 PASS: contains `lib.rs`; valid JSON |
| `task accept` | Atomically accept a task for Pixel-owned worker execution | A1 | A1 PASS: contains `accepted`; valid JSON |
| `task begin` | Begin a provider-neutral durable Pixel task ledger | A1 | A1 PASS: valid JSON |
| `task events` | Replay bounded, factual task-ledger events | A1 | A1 PASS: contains `task-1789188171-1`; valid JSON |
| `task plan-validate` | Deterministically validate a model-proposed work plan before fanout | A1 | A1 PASS: valid JSON |
| `task prepare` | Refresh a task's factual repository snapshot | A1 | A1 PASS: contains `task-1789188171-1`; valid JSON |
| `task race-poll` | Inspect a race and promote the first Pixel-eligible candidate, if any | A1 | A1 PASS: valid JSON |
| `task race-start` | Start a bounded race across pre-registered, isolated candidates | A1 | A1 PASS: valid JSON |
| `task reset` | Remove the current Claude session task packet | A1 | A1 PASS: valid JSON |
| `task sandbox-cancel` | Alias for sandbox cleanup when cancelling a candidate | A1 | A1 PASS: valid JSON |
| `task sandbox-cleanup` | Discard the recorded candidate worktree | A1 | A1 PASS: valid JSON |
| `task sandbox-create` | Create or reopen an isolated candidate worktree at current HEAD | A1 | A1 PASS: contains `sandbox_root`; valid JSON |
| `task sandbox-inspect` | Inspect a candidate without mutating either worktree | A1 | A1 PASS: contains `eligible`; valid JSON |
| `task sandbox-promote` | Compare-and-apply an eligible candidate; never performs a merge | A1 | A1 PASS: contains `promoted`; valid JSON |
| `task show` | Print the current Claude session task packet, if it is still valid | A1 | A1 PASS: contains `display-session`; valid JSON |
| `task status` | Print a durable task record | A1 | A1 PASS: contains `task-1789188171-1`; valid JSON |
| `task worker-start` | Start one Claude worker inside an existing Pixel-owned sandbox | A1 | A1 PASS: valid JSON |
| `task worker-status` | Report the recorded process truth for one Pixel worker | A1 | A1 PASS: contains `running`; valid JSON |
| `task worker-stop` | Stop one Pixel-owned Claude worker and its process group | A1 | A1 PASS: valid JSON |
| `trace` | Call path between two symbols | A1 | A1 PASS: valid JSON |
| `uninstall` | Remove everything `pixel install` wrote: managed blocks from agent-config files, hook entries from all settings files, hook scripts, the pi guard extension, the rule s… | A2 | A2 PASS populated/installed fixture; exact argv/exit in boundary observations |
| `update` | Fast-forward merge to a target OID (refuses non-ff + dirty intersection) | A1 | A1 PASS: valid JSON |
| `upgrade` | Rebuild the binary, stop the daemon, copy the new binary to the install path, and optionally restart the daemon | A2 | A2 PASS populated/installed fixture; exact argv/exit in boundary observations |
| `uses` | Direct callers or callees of a symbol | A1 | A1 PASS: valid JSON |


## Cargo integration inventory

`cargo metadata --no-deps --format-version 1` identified the packages and executable integration targets below. Unit tests live inside each package; a listed target is not by itself observed coverage.

| Package | Integration-test targets | Executable regression command | Observed result |
|---|---|---|
| `pixel-index` | Inline unit tests / library target | `cargo test -p pixel-index` | Coordinator final workspace: 865 tests / 60 suites PASS, exit 0 |
| `pixel-git` | Inline unit tests / library target | `cargo test -p pixel-git` | Coordinator final workspace: 865 tests / 60 suites PASS, exit 0 |
| `pixel-cli` | `ask_contract`, `flow_cli`, `upgrade_cli`, `guard_deny`, `json_contract`, `metrics_cli`, `post_edit_cli`, `rescue_cli`, `search_compat_cli`, `targets_cli` | `cargo test -p pixel-cli` | Coordinator final workspace: 865 tests / 60 suites PASS, exit 0 |
| `pixel-actionlog` | Inline unit tests / library target | `cargo test -p pixel-actionlog` | Coordinator final workspace: 865 tests / 60 suites PASS, exit 0 |
| `pixel-daemon` | `targets` | `cargo test -p pixel-daemon` | Coordinator final workspace: 865 tests / 60 suites PASS, exit 0 |
| `pixel-context` | Inline unit tests / library target | `cargo test -p pixel-context` | Coordinator final workspace: 865 tests / 60 suites PASS, exit 0 |
| `pixel-facts` | `excavate_rescue_v2`, `facts_integration` | `cargo test -p pixel-facts` | Coordinator final workspace: 865 tests / 60 suites PASS, exit 0 |
| `pixel-graph` | `changes_suggested_tests`, `concept_engine1_audit`, `concept_tests`, `import_resolution` | `cargo test -p pixel-graph` | Coordinator final workspace: 865 tests / 60 suites PASS, exit 0 |
| `pixel-ops` | `branches`, `crash_matrix`, `envfile`, `provenance`, `publish_property`, `reconcile_matrix`, `rewrite_matrix` | `cargo test -p pixel-ops` | Final workspace: 60 suites PASS, exit 0; default-scope publish regression covered |
| `pixel-proto` | Inline unit tests / library target | `cargo test -p pixel-proto` | Coordinator final workspace: 865 tests / 60 suites PASS, exit 0 |
| `pixel-rank` | `signals_tests` | `cargo test -p pixel-rank` | Coordinator final workspace: 865 tests / 60 suites PASS, exit 0 |
| `pixel-session` | `query_mcp`, `store` | `cargo test -p pixel-session` | Coordinator final workspace: 865 tests / 60 suites PASS, exit 0 |
| `pixel-recall` | `export` | `cargo test -p pixel-recall` | Coordinator final workspace: 865 tests / 60 suites PASS, exit 0 |
| `pixel-flow` | Inline unit tests / library target | `cargo test -p pixel-flow` | 36 passed, 0 failed (F3) |
| `pixel-install` | `install_tests` | `cargo test -p pixel-install` | Coordinator final workspace: 865 tests / 60 suites PASS, exit 0 |
| `pixel-bench` | Inline unit tests / library target | `cargo test -p pixel-bench` | Coordinator final workspace: 865 tests / 60 suites PASS, exit 0 |

## Boundary execution records

- **A1:** `python3 scripts/system_audit.py --pixel target/debug/pixel --output /tmp/pixel-system-audit-final.json` → **102/102 PASS**, exit 0; 87 unique leaves. Frozen binary SHA256 `7977b61b795ffd1ea985ad68718b6ea734846b1eede952e177f4f99f8f0ac340`.
- **A2:** `python3 scripts/system_audit_recall.py target/debug/pixel --output /tmp/pixel-recall-audit-final-current.json` → **28/28 PASS**, exit 0, same binary hash as A1. Populated malformed Claude JSONL, incremental append, temporary daemon lifecycles, configuration preservation and upgrade failure/success.
- **A3:** `tests/ask-ranking/verify.py` and `tests/ask-ranking/observed.json` identify a frozen 195-file corpus and earlier immutable binary `d94b0e0d1571400886bea2a5a8f9c9001940034e316fe880799346d2072b67f3`; manual questions rank **3/2**. Explicit 3/5/8 limits compared, retain eight. This earlier-candidate evidence is not relabeled as a final installed-binary run.
- **U1:** `cargo test -p pixel-cli --test upgrade_cli` → **2/2 PASS**: selected repo alone receives Shutdown; an unresponsive socket errors within the bound without false completion.
- Exact argv, expected output assertions, exit codes, durations and binary identity are retained in `tests/system-audit/2026-09-12-cli-boundaries.json`. Temporary paths identify disposable fixtures only.

## Cross-language, protocol, script and release boundaries

| Integration | Fixture and observed evidence | Limitation |
|---|---|---|
| Rust CLI → TypeScript sniper | Integration lane: 39/39 tests + tsgo exit 0; real error/run/event JSON, retention, unknown surface refusal | Final installed same-candidate gate recorded below |
| Vite plugin/client/reporter | Real local server: browser POST mapping, GET refusal, HTTP 500 excerpt, HMR and pid/port, included in JS suite | No personal or authenticated browser |
| Sniper → stdio MCP | A1 initialize, list exactly five tools, call all five against populated store, EOF exits 0 | Not every transport timing fault |
| Provider hooks → Pixel | A1 retired hook verbs answer nothing: exit 0, empty stdout | Protocol fixture proof is distinct from live installed host delivery; `task-event` is covered by the `pixel-cli` hook tests |
| Task workers/race | A1 all 18 leaves; real temp worktrees, fake process groups, accepted→prepare→worker running→stop, winner edit promoted and losers cleaned | No paid/authenticated provider execution |
| Flow → command sequence | F1–F3 real shell/fake browser, malicious-looking argument/comment data inert, JSON and success/failure/refusal | Actual browser intentionally substituted |
| Upgrade → daemon socket | U1 scoped Shutdown does not contact unrelated daemon; withheld response bounded with error | Not every filesystem crash point |
| Install/migrate | A2 repeated install and unknown-setting preservation, uninstall, failed build no mutation; migrate prepared=true/rebuilt=false | Mocked release installer 2/2; six shell scripts syntax only, not all network faults |
| CI/Linux release | Integration lane parsed YAML and inspected model2vec-only Linux feature/config flags locally | **No Linux runtime verification** on this Mac |
| Ranking/defaults | Frozen A3 queries; boundary/negation/tie/model tests in Rust; keep eight | Single latency samples do not prove statistical or agent-outcome non-regression |
| Broad relevance | `tests/system-audit/2026-09-12-relevance-audit.json`, frozen 14-source-file corpus digest and unchanged gates | **FAIL:** semantic imports source rank 14 / NDCG@10=0; concept resolve top-1=0/10 |

## Findings and residual release gates

- Fixed through reproduction and regression: shell argument/comment injection; flow JSON/dry-run/false-success; accepted-task preparation; upgrade global daemon termination and unbounded request; ready JSON pollution; migration false rebuild metadata; diff patch/metadata ref/path mismatch.
- Independent ranking review caught chunk boundary pseudo-words and empty-file zero-vector failure; full-file lexical tokens, empty-file coverage and preserved model selection were re-reviewed.
- Post-compaction correctly refused an older-HEAD fixture manifest after rewrite. A1 now explicitly reindexes history before creating current-head hints; no stale-manifest assertion was weakened.
- Initial fixture safety error: an early disposable-HOME imperative hook launched the host Claude CLI, which exited 2. It was checked/stopped; current A1 substitutes every provider/browser executable before hooks. No subsequent real provider execution is required.
- Broad relevance failures above remain visible release blockers; per-leaf invocation coverage is **not** an exhaustive correctness claim.
- Successful transcript model setup/embedding, Linux runtime, real authenticated provider/browser operation, every mutation concurrency/crash/timeout point remain unverified by these substitute/refusal fixtures.
- Final workspace, lint, features and activation gates are recorded below; relevance and reviewer-quota blockers remain.
- Claude review returned weekly-quota 429, not approval. Requested two-reviewer convergence is therefore not established by independent local reviews alone.
- Retain default eight: smaller limits lose required evidence, and representative agent-outcome/latency non-regression was not demonstrated.
- No claim that hidden defects do not exist is supported by this ledger.

### Follow-on relevance investigation (2026-09-12)

- Final installed release SHA256: `81f67e823965615cb5d39d6f203d333ae69f82560cba9d614d2b0d9a59d562bb`. Workspace test **875/60 PASS**, formatting **exit 0**, and warnings-denied Clippy **exit 0**. Reinstall plus parallel history index/config refresh completed; doctor reported **11 green / 0 yellow / 0 red**.
- A frozen 195-file disposable corpus and the preceding installed binary were retained before tuning. On the follow-on candidate, all five frozen setup controls retained their required evidence at limit eight; the manual guide remained **3/2**. Limits three and five still lose the excluded-directories control, so eight remains the default.
- Filename lexical evidence now uses only the basename before the first dot. A real-file regression reproduced and fixed the false `d` hit from `types.d.ts`; directory and extension components remain excluded.
- `resolve` previously passed database row ordinal to its reranker and applied requested limits before reranking. Regression tests now prove a production identifier and a word-intersection concept outrank an earlier test-path duplicate even with `limit: 1`; direct tiers scan a bounded 20,000-candidate pool and mark a full pool as capped.
- Benchmark response parsing now fails closed on missing, non-string, or blank match paths instead of silently promoting a later path to rank one.
- The strict graph benchmark's `ask` lane gets past all ten frozen queries, including the former `imports.rs` NDCG@10 zero. The resolver's original first failure (`concept index resolve phrase map marked`) is fixed by normalizing owner-name evidence to distinct query-word coverage.
- A separate adjudication found one frozen exclusive label incomplete: `resolution ranked candidate ambiguity disambiguation` directly describes `concept_resolve.rs::RankedCandidate`, while `resolve.rs` owns a distinct call-target ambiguity responsibility. Qrels v2 preserves the frozen query and legacy file while adding `concept_resolve.rs` as an audited second relevant owner.
- The owner-name hint was also corrected: it is now proportional to the number of distinct query words present in the owner identifier. A regression proves one owner word cannot outrank a concept with two content-word matches, while 0%, 25%, 50%, and 100% owner coverage remain deterministic. The formerly failing `concept index resolve phrase map marked` strict control now passes at rank 1.
- The mixed `callers callees impact trace reachability` control is inherently multi-owner (`impact.rs` for callers/callees and `trace.rs` for reachability); deterministic ranked output can validly return its first explicit filename owner. The benchmark retains its legacy top-1 label and separately records the semantic ambiguity.
- Bounded exact basename-component evidence now augments sparse T2 results, with no substring or stemming shortcut. Sparse means no current concept covers a strict majority of query words; an exact half-coverage row remains weak enough for a direct module title to compete. The pool is capped at 256 files, real rows are promoted rather than duplicated, and provenance is `filename component overlap: …`; synthetic filename-only rows carry line `0`.
- Regressions cover missing-target recovery (`impact.rs`), existing-row promotion (`cluster.rs`), sparse two-word and exact-half coverage, strict-majority suppression, and compound-extension rejection (`types.d.ts` does not match `d`). The isolated graph benchmark now reaches **100.0% (10/10) resolver top-1 correctness**, NDCG@10 **0.985 lexical ranked / 0.992 hybrid**, and exits 0.

## Final installed-candidate gate (coordinator-observed)

All following results are attributed to the coordinating agent, not inferred from earlier debug runs. Installed release SHA256: `d3ef0fa370bd9b42b8e03e08f5d7a24d9dfb21dc09b0f408e545dbf348a953b0`.

- Primary boundary audit **102/102 PASS**, recall/install audit **28/28 PASS**, TypeScript **39/39 PASS**, tsgo **exit 0**, all against this installed release.
- Workspace **865 tests / 60 suites PASS**; formatting **exit 0**; warnings-denied Clippy **exit 0**; three CLI feature checks **exit 0**.
- Atomic reinstall followed by parallel `index --history` and `build-agent-config && pixel install`: all **exit 0**. Doctor **11 green / 0 yellow / 0 red**. Only the repository daemon was restarted; ready JSON observed.
- The two original questions return manual guide at **3/2** in the final 195-file corpus after raw audit evidence was moved under excluded `tests/`. The installed binary was rerun after that move: both queries searched all 195 candidates without degradation and returned the default eight results. This confirms acceptance but does not erase the separate broad relevance failures.
- Parent-observed reports: `/tmp/pixel-installed-system-audit.json`, `/tmp/pixel-installed-recall-audit.json`; copies included in boundary artifact.
- Develop recreated from main base `d7f2d0a`; old develop `abf3c…` preserved in local and remote backups. [PR #24](https://github.com/Pixel-CLI/pixel/pull/24) remains open, unmodified by this task; no implementation push while relevance/review blockers remain.
