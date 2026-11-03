# `pixel dependency-swap` — bulk dependency migration across a big tree (design note)

Status: proposal, not implemented. Design for
[issue #432](https://github.com/Pixel-CLI/pixel/issues/432): a function that
bulk-replaces a dependency in a big tree — "migrate a library across all your
`~/`", e.g. TypeScript to tsgo (microsoft/typescript-go) or Lint to OXC
(oxc.rs) — using pixel's deterministic digging, over a shallow index that
ignores `node_modules` and the right folders, with an optional `pixel
classify` guard deciding which migrations are safe (a TS7 migration is less
attractive in a project that has not moved in 4–5 years). Every "today" claim
below was read in the code of this worktree.

## Ground truth: what pixel has today

| Fact | Where | Consequence for the design |
| --- | --- | --- |
| the CLI is noun-first kebab-case: `build-index`, `search-content`, `find-code`, `what-changed`, `web-search`, `review-gate`, `plan-rollback`, `index-pack`, `index-unpack`, `run-recipe`, `scope-task`, `prepare-repo`, `sync-branch`, `record-event` | `crates/pixel/src/main.rs` `enum Command`; the `Command surface` table in `ARCHITECTURE.md` | a new top-level op is one noun-first name with a noun → verb reading; `dependency-swap` fits (`web-search`, `index-pack` are the closest shapes) |
| `pixel rename` is the closest operative precedent: graph-resolved definition/call/reference/import sites, each **verified against a fresh tree-sitter parse before bytes are touched**; unresolved same-name sites are reported, never guessed; `--dry-run` prints the edit set without writing | `crates/pixel/src/main.rs` `Command::Rename`, `--dry-run` | the rewrite discipline the swap must inherit: verified per site, unresolved reported, dry-run first |
| text search is regex/literal over an indexed tree: `search-content <pattern> [paths…]` with `-g`/`--glob`, `-t`/`--type`, `-F`/`--fixed-strings`, `-l`/`--files-with-matches`, `--limit`; the repo root is discovered automatically for each path | `crates/pixel/src/main.rs` `Command::SearchContent` | the deterministic find phase is this search path (plan → resolve postings → verify), not a hand-rolled grepper |
| every index build walks against `DEFAULT_IGNORED_DIRS`: `.git`, `.pixel`, `node_modules`, `bower_components`, `Pods`, `vendor`, `target`, `_build`, `DerivedData`, `dist`, `build`, `out`, `.gradle`, `__pycache__`, `.venv`, `venv`, `.tox`, `site-packages`, `.terraform`, `.next`, `.nuxt`, `.cache`, `.npm`, `.yarn`, `.pnpm-store`; `PIXEL_INDEX_NO_DEFAULT_IGNORES=1` opts out | `crates/pixel-index/src/index.rs:30` (`DEFAULT_IGNORED_DIRS`, `is_ignored_dir_name`) | the "shallow" scoping already exists as the default walk policy; the swap must reuse it, not re-justify it |
| the repo boundary is git-first: `discover_root` returns the nearest `.git` and refuses to anchor on a journal-only `.pixel` (the global `$HOME/.pixel` state dir must never make a gitless invocation below it plain-walk-index the whole home) | `crates/pixel/src/main.rs` `discover_root` | a `~/`-wide op cannot rely on implicit rooting: the scope must be declared roots, never inferred ancestors |
| the index is git-anchored per root (base shard at a pinned commit, delta shard to HEAD, in-memory dirty overlay); a daemon per root is the singleton builder | `crates/pixel-index/` (`base.shard`, `delta.shard`, `state.json`), `Command::BuildIndex` daemon route (`Request::Reindex`) | the shallow home index is a `build-index` per declared root; working-tree edits inside a repo are visible to it via the overlay |
| code vectors are **never written when the search root is `$HOME`** | `ARCHITECTURE.md` on-disk state (`code-vectors/`: "Written only when the search root carries `base.shard` and is not `$HOME`") | the migration needs no semantic search; a shallow home index stays vector-free by design, so determinism is untouched |
| credential-shaped indexed paths must never be echoed or rewritten: `credential_path` (name/suffix based; `secrets`, `.env`, `id_rsa`, `*.pem` families) | `crates/pixel-index/src/index.rs` `credential_path`; `search_compat::credential_path` delegates to it | `--apply` must refuse these files even when a spec matches |
| `pixel classify` decides a bounded label set through an LLM: `--label`, `--criterion label=…`, `--context`, `--jsonl`, `--engine remote\|ollaya`, `--if-warm`; the output always discloses `snapshot.deterministic=false`; the local/static backends were **removed** — no off-the-shelf local model beat Jev on the coding benchmark (`docs/bench/decide-bakeoff.md`) | `crates/pixel/src/classify.rs` (`ClassifyOptions`, `REMOTE_BASIS`), module doc header | classify is an advisory vote that must be truthfully labeled non-deterministic; when it is off, the gate must be a deterministic function of the same evidence |
| classify is disabled by default (`pixel config classify on\|off`, "disabled by default"); the engine preference is stored per install | `crates/pixel/src/main.rs` `Command::Config`/`Classify`; `config.yaml` comments | default-off matches repo convention: `--classify` is an explicit flag, never a default-on dependency |
| per-repo provenance exists as a precedent: `pixel workspace` fans `impact`/`who-calls` across registered repos with per-repo provenance; `pixel plan-rollback` is the surgical revert planner | `ARCHITECTURE.md` Command surface | the swap's per-repo report/confirmation unit and its rollback pointer have home-grown precedents |

## Naming

`pixel dependency-swap <from> <to> [path]`.

The object is a dependency — a declared library name (a `package.json`
`dependencies` key, a `Cargo.toml` `[dependencies]` name, an import/require
specifier). Noun first, like `web-search`, `index-pack`, `what-changed`:
the thing being operated on precedes the act. `swap` is chosen over
`migrate` (which promises planning/sequencing pixel does not do) and over
`replace` (a plain textual find/replace; the op is spec-aware). `dep-swap-*`
shortens inside the plan when the op composes with `plan-rollback`.

Key flags:

| Flag | Meaning |
| --- | --- |
| `from`, `to` (positional) | the declared dependency spec to leave and the one to land on, e.g. `typescript` → `tsgo` |
| `path` (positional, default `.`) | the root of the big tree; nested git repositories below it are discovered as the unit of per-repo confirmation (not invented ancestors) |
| `--mode all\|manifest\|imports` (default `all`) | which sites to rewrite: manifest key/value updates, import/specifier occurrences, both |
| `--classify` | ask `pixel classify` per repository whether the migration is a safe bet (see guard) |
| `--min-activity <months>` (default 24) | deterministic stale-project floor: a repo whose last commit is older than this is dropped from the plan |
| `--apply` | perform the edits; without it the command runs dry and prints the edit set (default) |
| `--confirm` | per-repository interactive confirmation before `--apply` |
| `--json` | machine-readable per-run report (plan, verdicts, counts) |

## Deterministic digging: the shallow index the operation runs on

`dependency-swap` never scans the raw tree and never re-implements rg. It
reads the same artifacts every deterministic pixel path reads:

1. **Declared scope, never implied.** The user names roots (`path`,
   repeatable). Each root is a `discover_root` boundary: a git repo root or
   an explicit non-git directory. The op refuses to root an unindexed
   ancestor — the exact hazard `discover_root`'s `$HOME/.pixel` guard
   exists to prevent (a journal-only `.pixel` at an ancestor must not make
   a gitless invocation plain-walk-index the home directory).
2. **Shallow index by default.** Each declared root gets a `pixel
   build-index` walk governed by `DEFAULT_IGNORED_DIRS`: `node_modules`,
   vendored trees (`vendor`, `Pods`, `bower_components`), build output
   (`target`, `dist`, `build`, `out`, `_build`, `DerivedData`, `.next`,
   `.nuxt`), language caches (`.venv`, `venv`, `__pycache__`, `.tox`,
   `site-packages`, `.gradle`, `.cache`, `.npm`, `.yarn`, `.pnpm-store`),
   VCS internals (`.git`, `.pixel`). That is precisely the "shallow index
   that ignores node_modules and the right folders" the issue asks for —
   it already ships as the walk policy. The shallow home index is a set of
   per-root shards (never a plain walk of the whole home), and it stays
   vector-free: pixel never writes code vectors for `$HOME`, so no
   embedding step can drift into the find phase.
3. **Find phase = the search path.** Candidate files come from
   `search-content`-style regex or fixed-string queries per site kind: the
   manifest-key query (`"typescript":` in `package.json`, `^typescript\b`
   in a `Cargo.toml` `[dependencies]` block, `typescript` in a `go.mod`
   require line) and the specifier query (`from 'typescript'`, `from
   "typescript"`, `require('typescript')`), narrowed with `-g`/`-t` so
   extension and glob shape the hit set exactly like the existing command.
   Postings are resolved from the shard and **verified against the live
   file** before they enter the plan — the search path's final shape
   (`plan → resolve postings → verify`), the one the shipped searches
   already use.
4. **Rewrite phase inherits `rename`'s discipline.** Every planned edit is
   re-read and re-parsed before its bytes are touched; a file whose content
   hash no longer matches the plan (changed since indexing) is dropped from
   the plan and reported, never rewritten blind. Unresolved specifier
   sites (a `typescript` string that is not an import or a declared key)
   are listed in the report, never guessed at.

## The optional `classify` guard and its deterministic fallback

The issue's example: a tsgo migration is less attractive in a project that
has not moved in 4–5 years. Pixel's answer is two layers, both running on
the **same evidence**, with `--classify` as the only non-deterministic
layer.

Evidence computed per repository, deterministically, from the shallow index
and git metadata:

- `site_count` — declared-key + specifier occurrences found in the index.
- `freshness_months` — months since the repo's last commit (`pixel
  commit-history` reads the same facts index).
- `already_moved` — whether `to` already appears anywhere in the repo.
- `dep_profile` — whether `from` is a runtime, dev-time or build-time
  dependency per the manifest section that declares it.
- `blockers` — boundary refusals (credential-shaped files, paths outside a
  declared root, unindexed trees) that would force dropping some sites.

**Default path — classify off.** A pure deterministic gate decides, and the
decision is documented as a threshold function, not a model. The default
rule: drop a repository whose `freshness_months` exceeds `--min-activity`
(default 24); drop a repository with a non-empty `blockers` set; sort the
rest descending by feasibility defined as `site_count ÷ (1 +
freshness_months / 12)` with `already_moved` as a tie-break. Same inputs →
same plan, byte for byte, with no network and no daemon dependency. This is
what `--dry-run`/`--apply` execute when `--classify` is not passed.

**`--classify` path.** One `pixel classify` decision **per repository**,
not one for the whole tree (a `~/` migration mixes moribund and active
projects). Each call is a bound-option Spec: `--label migrate --label
defer`, `--criterion migrate=…`/`--criterion defer=…` spelling the rubric,
`--context` the shared framing, and the repository's evidence as `text`.
The verdict is advisory and disclosed honestly: it is non-deterministic,
network-bound, engine- and prompt-dependent, and the output says so
(`snapshot.deterministic=false`) exactly as `pixel classify` already does —
the swap never routes through classify with `--if-warm` pretending
otherwise. Concretely:

- classify decides a candidate at the **margin** — a repo past the
  deterministic floor (`--min-activity` already dropped it) or one whose
  `dep_profile` is ambiguous (a build-time dep that a fixture or script
  hard-codes). It can *defer*, never *compel*.
- a `defer` verdict adds the repository to a `deferred:` section of the
  plan; `--defer-each` is still required to skip it at `--apply` time.
- the engine comes from stored preference or `--engine`; `--if-warm` is
  honored (a cold local engine → the deterministic gate answers, the plan
  notes "classify unavailable").
- reproducibility under `--classify` is explicitly out of scope: the plan
  records the model id and the evidence snapshot so the run is *auditable*,
  which is the honest claim. Determinism is the default path's promise, not
  the classify path's.

## Safety contract

1. **Dry-run is the default.** Without `--apply` the command computes,
   verifies and prints the edit set per repository (file, line, old → new
   with per-site verification status) and exits 0. Nothing is written.
2. **Declared boundaries only, enforced twice.** A site is eligible for
   `--apply` only if (a) its path sits inside a declared root, (b) it is
   not under any `DEFAULT_IGNORED_DIRS` directory, and (c) it is not a
   `credential_path` file. The dry-run list is produced against the live
   tree; at apply time the same three checks re-run per file — a file whose
   path drifted between plan and apply is reported and left alone.
3. **Per-repo confirmation.** `--apply` without `--confirm` rewrites
   nothing; the user confirms per repository (or passes `--defer-each` to
   name the repositories explicitly). The unit of confirmation is the
   repository, because the rollback unit is the repository.
4. **Rollback/report shape.** `--apply` writes only git-visible working-tree
   edits inside git repositories; non-git directories are refusal sites
   (no VCS → no rollback path, therefore never rewritten). The run writes
   `.pixel/actions.jsonl`-style provenance plus a per-run report
   (`<root>/.pixel/deps-swap/<ts>.jsonl`, one line per edit with old/new
   content hashes) so `pixel plan-rollback` — the existing surgical revert
   planner — is the advertised rollback, and `pixel diff`/`pixel
   review-changes` show the working-tree delta. No side copies are
   invented; git is the one source of truth.
5. **Nothing outside the tree.** No config file, no global prompt, no home
   hook is touched unless it sits inside a declared root; `~/.pixel` state
   and `DEFAULT_IGNORED_DIRS` trees are never candidates.

## Defaults that bind the first delivery

- `pixel dependency-swap typescript tsgo ~` with no further flags: prints a
  plan over every git repository under `~/` whose shallow index exists or
  can be built, applies the deterministic gate with `--min-activity 24`,
  and exits 0 having written nothing.
- `--apply --confirm` is the smallest mutating invocation.
- `--classify` only ever votes at the margin; it never turns the op
  non-deterministic by default.

## Open questions to resolve before implementation

1. **Root discovery granularity.** One positional `path` (per `rename`) or
   repeatable roots (per `search-content`)? Recommendation: one positional
   `path` default `.`; nested git repositories below it are the
   confirmation units, mirroring `pixel workspace`'s per-repo provenance.
2. **Manifest v. imports fidelity.** Should a `package.json`
   `"dependencies": { "typescript": "5.x" }` update land on the key
   (`typescript` → `tsgo`) or the version range, or both, and should the
   specifier rewrite extend to quoted strings inside other manifests the
   index walks? Recommendation: structural per-format manifest edits
   (key plus declared range) and literal specifier rewrites in indexed
   source; both behind `--mode`.
3. **The stale-project floor.** `--min-activity` default: 24 months
   recommended. The issue's 4–5-year example is the tail of the same
   distribution; expose the knob, keep the default conservative but
   usable. `--min-activity 0` disables the gate.
4. **Dirty working trees.** May `--apply` touch a repository with uncommitted
   changes in files outside the plan? Recommendation: yes — edits are
   git-visible overlays — but any planned file whose content hash no longer
   matches the plan is re-verified and dropped, and the report counts
   dropped files. A *clean* requirement would defeat the op's job (the whole
   point is touching many repositories at once).
5. **Landing mechanics.** The `Command surface` table, `pixel --help`, and
   the agent prompt must agree the moment the command exists
   (`docs_drift` enforces it), and the shallow-home-index semantic (a
   vector-free `$HOME`) deserves a sentence in `ARCHITECTURE.md`. The
   precedent from `rename` applies: the op is a working-tree mutation, so
   the mutation gate's verdict is part of the definition of done.