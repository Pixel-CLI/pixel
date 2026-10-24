# Pixel identity and interface inventory

The rewrite is now a candidate in
[Pixel PR #1343](https://github.com/Pixel-CLI/pixel/pull/1343), on
`codex/pixel-rto-replacement`. Active changes belong to that Pixel branch;
the RTO preparation is historical provenance, not another required workstream.
The original Pixel checkout and its Git identity remain intact. This candidate
does not publish a release or change version `0.0.0`.

Pixel speeds up coding-agent work by having deterministic mechanisms do
predictable work. Retrieval, the graph and the brief remain the rewrite's core.
Token savings are a consequence. Git and browser workflows remain separate
plugins; the previous Pixel engine's CLI/API and removed features are not
compatibility requirements.

## Identity and interfaces

The RTO column describes the source baseline at
`69c8ea559b4c1d6b79f56a154d9b746820680f85`. The Pixel column describes this
candidate, not an assertion that the previous Pixel engine had the same API.

| Surface | RTO baseline | Pixel candidate / consequence |
| --- | --- | --- |
| Product | RTO / Rust Token Obliteration | Pixel; thesis centers on faster agent work |
| Binary and Cargo package | `rto` | `pixel`; no old-name alias |
| Side build | `rto-dev` | `pixel-dev`; isolated install safeguards remain |
| Workspace | `crates/rto` and 10 `rto-*` crates | `crates/pixel` and 10 `pixel-*` crates; Rust imports use `pixel_*` |
| Version | `0.0.0` | `0.0.0`; publication/version selection excluded |
| Commands | Rewrite's query, brief, hook, setup, config, doctor, metrics, plugin and daemon commands | Same rewrite architecture and command semantics, under `pixel`; previous Pixel aliases are not restored |
| Hook entry point | `rto hook EVENT --host HOST` | `pixel hook EVENT --host HOST`; hooks remain part of the product |
| Environment | Product `RTO_*` variables | Corresponding `PIXEL_*` variables, including build provenance, brief switches, daemon, metrics and plugin environment; provider credentials keep their existing names |
| Repository state | `.rto/` | `.pixel/`; graph, index, history, sessions, metrics and config agree on the new sidecar |
| Global state | `~/.rto/` | `~/.pixel/`; receipt, config, plugin registry and trust store use the new identity |
| Cache and shared assets | XDG cache / local-share `rto` directories | Corresponding `pixel` directories; no automatic move or deletion of RTO state |
| Daemon endpoints | `rto-<hash>` sockets / PID / lock paths | `pixel-<hash>` equivalents, including owner-checked fallback directory; wire operations keep the rewrite's contract |
| Injected context | `[RTO:BRIEF]`, `[RTO:EDIT]`, `[RTO:STALE-CALLERS]` | `[PIXEL:BRIEF]`, `[PIXEL:EDIT]`, `[PIXEL:STALE-CALLERS]` |
| Metrics and diagnostics | `🟩 rto`, `rto:` | `🟩 pixel`, `pixel:`; measured values and workflow estimates stay distinct |
| Host integrations | RTO hook registrations, Pi package/extensions, OpenCode/Antigravity assets | Pixel-owned registrations/assets, receipt-backed setup/remove; foreign settings preserved |
| Setup migration sweep | Removes competing Pixel hooks, packages, plugins and known skills | Removed; Pixel must not sweep away its own install or classify existing commands as foreign-product debris |
| Setup repeated / removal | Managed receipts, backup/restore, idempotent registration | Same ownership guarantees; regression cases use fixture HOME directories |
| Doctor | RTO install checks plus competing Pixel migration warnings | Pixel install checks; competing-Pixel warnings and cleanup repairs removed |
| Skills and Pi tools | `rto`, `rto-classify`, `/rto-impact`, `ask_rto_*` | Pixel identities; old engine capabilities are not added back |
| Plugin manifest | `rto-plugin.toml` | `pixel-plugin.toml`, same API 1 schema; external plugins need the new names/environment |
| Plugin discovery | `.rto/plugins`, `~/.rto/plugins`, `rto-NAME` on PATH | `.pixel/plugins`, `~/.pixel/plugins`, `pixel-NAME` on PATH; trust validation remains required |
| Index bundles | `.rtopack`, `rto_version`, `packed_by_rto` | `.pixelpack`, `pixel_version`, `packed_by_pixel`; no claim of old-format compatibility |
| Semantic vector segments | 8-byte `RTOVECS1` magic | 8-byte `PIXVECS1`; header width and offsets stay unchanged |
| Install/update source | `rto-cli/rto` | `Pixel-CLI/pixel`; release archives/binary paths use Pixel |
| GitHub Action | `.github/actions/setup-rto` | `.github/actions/setup-pixel`; downloads the Pixel release contract |
| Distribution | The historical source included formula/tap machinery | Release archives, installer and source builds remain; formula/bottle generation and publication are removed |
| CI and release machinery | RTO package/path/artifact selectors | Pixel selectors; no release, package publication or merge is performed by this preparation |
| Website and examples | RTO active copy, commands and links | Existing Pixel site at `https://pixel-cli.dev/` and README retained, with explicit install-channel corrections; historical media remains labeled |
| Provenance | Pixel source `e8fd304`, RTO contributors, measured historical runs | Preserved; renaming a product does not change authorship, source revision or measured evidence |

## State and upgrade boundary

Existing `.rto/` and `~/.rto/` state is left in place. The rewrite does not
silently import the previous Pixel engine's configuration, hook commands,
plugin binaries or graph format. The `.pixel` name is shared with that previous
generation: keep a backup of valuable previous state before deploying this
candidate, and explicitly configure this generation's hooks and plugins. Its
normal stores reject or rebuild incompatible cache schemas under their existing
contracts; this is not a data migration guarantee.

Setup only manages the entries and assets it owns. Existing command text that
merely contains `pixel` is not sufficient ownership evidence. A repeated setup
must not double-register its hooks, remove itself, or erase unrelated hooks and
settings. Removal restores backed-up content where applicable and keeps user
edits according to the receipt contract in `../specs/setup-migration.md`.

## Useful behavior from the previous Pixel checkpoint

The inspected checkpoint is `d7d1484fa128461e13c021641ce3a17b64952955`
(five files, 143 additions and five deletions). The old checkout later merged
upstream at `616c93d4dfd07b4c3cfe2c3b6324e52a4a0192d9`; neither checkout nor
checkpoint is pushed or changed by this preparation.

| Previous change | Equivalent in the rewrite before adaptation | Confirmed gap / candidate action | Verification boundary |
| --- | --- | --- | --- |
| `PIXEL_DAEMON_IDLE_SECS` | `serve::run_loop` used a fixed 30-minute timeout | Added positive trimmed seconds, 1,800-second fallback for missing/invalid/zero values, used for both wait and idle exit | Source-extracted helper assertions pass; integrated regression added but not run |
| Definition lookup without usage blocks | Region packing always allowed usage sites | Single resolved definition omits usage blocks for lookup-only prompts; edits and explicit references keep them | Source-extracted intent assertions pass; Plan/packing regressions added but not run |
| Pi collapsed summary and click / ctrl+o expansion | Already present in `assets/pi-pixel.ts` | Kept the existing renderer; no duplicate implementation | Source-equivalence inspection; no live Pi session run |
| Pi expanded skill-envelope prompt extraction | Already present before invoking the brief | Kept extraction of the user's arguments from the skill envelope | Source-equivalence inspection; no live Pi session run |

These changes do not bring back the previous engine's architecture, CLI or
removed workflows. Historical checkpoint timing/memory numbers are not new
measurements of this candidate.

## Historical source-preparation evidence (2026-10-24)

The following records the source preparation before the Pixel import. It is
not validation of subsequent Pixel PR revisions. Current import and validation
evidence belongs to `docs/imports/rto-pixel-2026-10-24.md` and Pixel PR #1343.

- `rtk cargo fmt --all -- --check`: exit 0.
- `rtk cargo metadata --no-deps --locked --format-version 1`: eleven Pixel
  packages, all `0.0.0`.
- `rtk python3 scripts/ci/check-spdx.py` and
  `rtk python3 scripts/ci/verify-action-pins.py`: exit 0; twelve workflow files
  have every `uses:` pinned.
- Each `scripts/test-*.py`, plus the Docker smoke runner's `test-runner.py`,
  executed with Python: seven suites, 81 tests, zero failed suites. These are
  script/fixture contracts, not an actual Docker or model campaign.
- A tiny `rustc --test` harness extracted the current timeout and lookup-intent
  functions from source: two tests, 17 input cases, zero failures. This is not
  a workspace compilation or an integrated region-pack/daemon test.
- Bun's transpiler parsed the four setup JS/TS assets without loading their
  imports or executing host code. No Pi/OpenCode session was exercised.
- A comment-normalization error during preparation was recovered in 36 setup
  files. Recovery decoded only the encoded comment regions; replaying the
  exact logged transformation reproduced each damaged file byte-for-byte.
  All 36 committed raw Git blobs matched the recovered SHA-256 manifest.
  Source review against the baseline found no unexplained lost behavior.
- The first published head, `492b3b2159f27a84f3145f243cdccd81418e69ba`,
  triggered [CI run 40124264893](https://github.com/rto-cli/rto/actions/runs/40124264893).
  GitHub refused to start its jobs because account payments failed or the
  spending limit needs increasing. Its jobs have zero executed steps; their
  failure status is not a test result. The final head's status is recorded on
  [PR #70](https://github.com/rto-cli/rto/pull/70).

Workspace Rust tests, clippy, dependency checks, cross-builds, site generation,
visual verification, real-host installs and comparative benchmarks remain
unverified. No check, protection or workflow was disabled to evade the billing
block. The candidate is not approved for release by these checks.

## Historical references and evidence

- Existing SPDX/copyright holders, license notices and pinned source hashes
  retain their original text.
- `CHANGELOG.md`, original lifting directions and recorded benchmark commands
  retain their historical product identity.
- Pixel 0.9.0 arena results and the 2026-10-22 RTO Ruby gate are historical
  evidence. Comparative benchmarks for this candidate have not run; neither
  parity nor a speedup over the previous Pixel engine is established.
- Historical screenshots/animations do not verify the renamed interface.

The install/update URLs require a release with this generation's layout;
existing older Pixel releases do not prove that contract. This candidate removes
the package-manager distribution channel. Pixel has an existing project board,
but the previous board-sync workflow is not restored.

Arena is restored as a developer skill and adapted to this engine. Pixel Retro
remains outside the core: its future diagnostics belong to
[pixel-recall#2](https://github.com/Pixel-CLI/pixel-recall/issues/2). Conversation
recall stays in that plugin; semantic search over code stays in the engine.
Task/task-state and the removed retrieval benchmarks remain absent.

The replacement lives in an isolated Pixel worktree. The original checkout at
`/Users/livio/Documents/pixel` and its checkpoint commits are unchanged; no main
push, release or HOME installation is part of this work.
