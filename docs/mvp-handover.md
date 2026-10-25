# MVP maintenance handover

Pixel is the maintenance home of the MVP foundations (ADR-0036). The last
import of model, store and index is pixel-mvp `e91e8da`; the lexicon is from
`35aaf1c` and is byte-identical to the lexicon at `e91e8da`.

## Owned here

- `crates/mvp-model`, `mvp-store`, and the indexer as pixel-graph's
  `mvp_index` module (`crates/pixel-graph/src/mvp_index`): edit directly,
  test in this workspace, include in the scheduled mutation campaign. ORIGIN.md records
  provenance, not a restriction. The copy-over script is retired.
- French prompt rules and BM25: maintained in `execution_brief/french.rs`
  and `words.rs`; lexicon data and its generator in `crates/mvp-lexicon`.
- Plugin clone restrictions and executable-bit trust hashes, and fail-open,
  single-JSON-line hooks: already implemented and tested in Pixel.

Keep crate names, graph paths and the origin commit unchanged during the
handover. When extraction changes, bump the relevant version in `mvp_index`;
when storage changes, bump mvp-store's schema as required. The adapter's
source signature includes the extractor versions. A Ruby merge rule change
also updates its undo format when required. No private checkout is needed.

## Regression checks

Pixel's `mvp_integrity` integration tests exercise the production refresh
adapter for mixed Ruby/Rust edits, corruption recovery and provenance without
rewriting git's index. The original library tests remain beside their code.
`mvp-reference.yml` runs the imported C-10 truth test on both original pinned
public repositories. Binary-specific MVP install/telemetry/selector tests
are not copied into an unrelated CLI; Pixel keeps its own entry-point tests.

## Product work continues in Pixel

- Ruby hybrid stays opt-in. Its refresh can delay a brief after an edit;
  improve daemon responsiveness and incremental work before enabling it (#1040).
  The recorded experiment is pixel-research-mvp's
  `evaluation/results/2026-10-16-ruby-hybrid-daemon.md`.
  Its rerun on Pixel main, `docs/bench/ruby-hybrid-daemon-2026-10-18.md`,
  still loses every brief sent 1 s after an edit on gumroad. MVP #111 was
  reassessed on main in `docs/bench/ruby-hybrid-correctness-2026-10-18.md`:
  no wrong target in the merge's judged edges, one #111 case open (#1218).
- Setup preservation and complete removal from #1011 are implemented by
  #1012, #1013, #1014, #1015 and #1019, including byte-exact restoration,
  owned-file cleanup, retries and symlink preservation. #1043 carries
  staged-file synchronization from MVP #85 into Pixel's writers.
- `mvp_index` still invokes git outside GitRunner, through its own
  `bounded` module (a timeout and an output cap on every command, #1085).
  The imported lint baseline can be tightened here without reopening the
  experimental repository.

## Deliberately retained product architecture

Pixel keeps its daemon, brief, plugin protocol and per-agent setup code.
The standalone MVP minimap/selector, classify CLI, plugin supervisor and
telemetry service are not imported wholesale. Their useful guarantees are
ported against Pixel's actual entry points. A harness-table refactor is not
required for the handover: Pixel supports integrations the MVP table does
not describe. It can be evaluated as a Pixel feature independently.

The evaluation protocol and historical evidence remain in
Pixel-CLI/pixel-research-mvp. ADR-0043 measures Pixel with the imported parts
on and off; it does not require maintaining a second product binary.

## Archive checklist

- The maintenance handover consists of #1039 ownership, #1041 lexicon tooling,
  #1042 index regression checks and #1043 settings synchronization.
- Keep unresolved Ruby work tracked in Pixel (#1040); #1011 covers the
  completed installation-preservation work.
- Resolve the disposition of MVP #85 and open tracker issues; archive the
  MVP with a pointer to Pixel only after the migration PRs merge.
- Retire any deployed MVP hooks/package separately, preserving user config.
  Archiving a repository does not uninstall an existing binary or service.
