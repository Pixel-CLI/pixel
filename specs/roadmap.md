# Roadmap, acceptance and process

Status: normative for sequencing. Detail beyond this file lives in the tracking
issues (an epic plus one per milestone).

## Sequencing

| M | Content |
|---|---|
| M0 | Skeleton, CI, process files, these specs, tracking issues; W1 (carry `website/` and `docs/motion/`) |
| M1 | `pixel-io`, `pixel-model`, `pixel-proto`, `pixel-metrics` |
| M2 | `pixel-graph`: store schema v1, engine core with language skip, Rust, `names.rs` + TS/JS, Python/Go/Java/C#/generic, Ruby + latency gate, concept extractors, `shape.rs`/`edit.rs`, analyses; nightly mutation workflow |
| M3 | `pixel-index`: trigram index, history, `semantic` feature |
| M4 | `pixel-daemon`, query CLI with 🟩 lines, `prepare`/`status`/`doctor`, `ARCHITECTURE.md` and `docs_drift` |
| M5 | `pixel-brief`: prompt/gate/route, locate/pack/render/sent/parts, recipes, `Op::Brief`, journal, probe runner |
| M6 | `pixel-setup`: host table, `pixel hook` (prompt, post-tool, relays), Claude/Codex/Devin/Gemini/Antigravity, the pi package, OpenCode plugin, skill deployment, install/uninstall/doctor, receipt-backed ownership |
| M7 | `EditCheck`/`StopCheck` and edit-input parsers (off by default), `pixel metrics`, the edit-task A/B |
| M8 | classify (all engines, `pixel config` keys, skill, pi tools), plugin host CLI and plugin skills, workspace and index pack/unpack |
| M9 | install.sh, release workflows (SLSA, SBOM), `setup-pixel` action, README, SECURITY and threat model, website W4, release evidence → `v0.1.0` |

## Implementation status

The candidate contains the M0–M8 implementation and M9 machinery (release
workflows, `check-release`, install script, `setup-pixel` action, README,
SECURITY, threat model and website). Publication of this Pixel PR is not a
merge, release or successful CI run. Deliberate departures from the sequencing table:

- M3's `semantic` feature is Model2Vec only and off by default (`specs/semantic.md`).
- M2's concept extractors became `pixel find-code`, which ranks lines of the text
  index by kind and keeps no concept table (`specs/graph.md`).
- The edit hooks stay off by default until the edit-task A/B below.

The existing Pixel website, domain, palette and type are retained. This
candidate updates only installation statements affected by explicit product decisions.

## Comparative evidence and release acceptance

No comparative run has been completed for this rewrite. Renaming the product
does not establish performance parity or change the historical baseline.
An existing comparative run may be cited for the exact versions it measured;
this transformation does not require another benchmark campaign. Its final
execution check must identify and exercise the published Pixel head rather
than substitute an older binary's results.

The restored `.agents/skills/arena/` orchestrates Claude Code and Codex trials
against the current Pixel command and hook contracts. Follow that skill's
bounded cases and explicit binary selection; the previous engine and a legacy
command shim are not prerequisites.

Record tool calls, provider-reported tokens, elapsed time and answer quality
with pinned prompts, repository revision, binary identity and commands
(`measuring.md`). The historical Pixel 0.9.0 figures remain evidence for their
original sample, not a release threshold for this candidate. Selecting a
representative comparison set and release acceptance thresholds remains a
maintainer decision. Edit hooks additionally run an edit-task set (signature change
with N callers, remove a parameter, rename a field): arms vanilla, brief, brief +
edit; they default on only on a correctness or turn gain.

## Historical lifting rules (RTO development)

The following records the original port from Pixel to RTO; these rename
directions are historical, not instructions for the current product.

- Source: Pixel at `e8fd304`, copied from that commit, edited in the same PR.
- The PR body lists each `lifted: <pixel path> → <rto path>` and what changed.
- Tests move with their code; the PR reports the lifted test count.
- Renames: `pixel_*` → `rto_*`; `.pixel/` → `.rto/`; `~/.pixel` → `~/.rto`;
  `[PIXEL:BRIEF]` → `[RTO:BRIEF]`; `PIXEL_*` → `RTO_*` only for the five
  surviving switches (`RTO_BRIEF`, `RTO_EDIT_HOOKS`, `RTO_DAEMON_AUTO_START`,
  `RTO_LOG`, `RTO_METRICS`, plus `RTO_METRICS_ROUND_TRIP_MS`); every other
  knob becomes a typed field of `~/.rto/config.yaml` / `.rto/config.yaml`.
- One tokenizer/stemmer, one BM25, one RRF, one `is_test_path`, one French
  lexicon live in `rto-model`. A named constant is the only spelling of a limit.
- Anything without a spec stays in Pixel's history.

## Current source identity

The product, binary, workspace packages, environment controls and state paths
use Pixel (`pixel`, `pixel-*`, `PIXEL_*`, `.pixel/`). Existing RTO state is
left untouched. The rewrite keeps its own CLI and API, without restoring the
previous Pixel engine's command surface. See `../docs/pixel-identity.md` for the
interface inventory and explicit migration boundary.

## Risks

Gate drift (lift `keyword_weight` byte-identical, parity fixtures); Rust steps
losing to the Python probes (ablation flags, external replay); wrong edges in
name-tier languages (Proved only for T0/T1 without a receiver); noisy or slow
edit hooks (no link on the hook path, shape change with callers only, off by
default); host contract drift (one host table, golden tests, install smoke per
release).
