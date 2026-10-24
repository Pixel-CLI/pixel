# Keeping ARCHITECTURE.md True

Always loaded: `ARCHITECTURE.md` is the map a contributor reads before
touching two crates, so a change that moves what it describes updates it in
the same pull request, not in a later clean-up. The 2026-09 audit found the
Claude integration still described as the retired `claude()` wrapper, the
graph file under the name it had before `graph.v2.db`, and two `.pixel/`
files nothing writes.

Tests hold part of the map (`cargo test -p pixel --test cli
docs_drift::`):

- `## Command surface`: one row per subcommand, in `pixel --help` order;
- `## Crates`: one row per workspace member, and the `Depends on` cell equal
  to the `pixel-*` entries of that crate's `[dependencies]` (and of
  `[dev-dependencies]` when the cell lists `(dev: …)`).

The rest is prose no test reads. When the diff does one of these, edit the
section named beside it:

| The change | Section |
| --- | --- |
| adds, renames or stops writing a file under `.pixel/`, `~/.pixel/`, `~/.local/{share,state}/pixel/`, or changes its owner | `## On-disk state` |
| changes the socket format, the envelope, an op's invariants, or `PROTOCOL_VERSION` | `## Daemon and wire contract` |
| changes how the CLI reaches the daemon, the output cap, or a truncation shape | `## Request path from the CLI` |
| changes index, graph or history freshness, a rebuild threshold or its env var | `## Indexes and freshness` |
| makes `pixel setup` (global or `--repo`) write, stop writing or move a file, hook or managed block, for any agent | `## Agent integration` (and the `pixel-install` row of `## Crates`) |
| adds, removes or re-scopes a `run-hook` entry point | the hook table in `## Agent integration` |
| changes a metrics estimator, its version or its defaults | `### Invocation accounting and chat delivery` |
| adds, removes or moves a CI job, a gate step or a workflow | `## Testing and gates` |
| changes `check-release`, `build.rs` or the default features | `## Release gate`, `## Build provenance`, `## Build features` |

- **Name the code, not a memory of it.** Quote the constant (`GRAPH_DB_FILE`,
  `PROTOCOL_VERSION`), the path and the env var as the code spells them, and
  check each against the tree before the push.
- **Delete what stopped being true.** A paragraph describing a retired
  mechanism "for history" is read as current; the history lives in the
  changelog and `git log`.
- **The pull request says it.** List `ARCHITECTURE.md` under "Docs touched",
  or state why the change moves nothing the map describes.
