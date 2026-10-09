# `pixel setup` — port of `but agent setup` (plan)

Status: in progress on `chore/setup`. Issue: #890. Companion reference:
[but-agent-setup-reference.md](but-agent-setup-reference.md) (what GitButler's
wizard does, read in their tree at `5cbe33d` and PR #14425).

## Answers (decided 2026-10-09)

1. **Agent list** — as `but` does it: specific per-harness targets, each with its
   own env markers, `$HOME`/repo config markers, instruction file and artifacts,
   plus a generic `AGENTS.md` target for a detected agent that maps nowhere.
2. **Feature list** — the twelve in the table below.
3. **Feature semantics** — see "What a feature writes" below; still open with the
   maintainer whether a feature that implies a hook deploys it or reports it.
4. **Scope** — `--repo <path>`, defaulting to the machine. In repo scope both the
   detection base and the write targets are the repository, not `$HOME`.
5. **`pixel config setup`** — separate command, untouched. No absorption, no
   hidden alias.
6. **Golden files** — `tests/setup/`, a folder about this feature only:
   `tests/setup/.agents/<agent>.md` (the exact instruction file the setup writes
   per agent, default features, fresh file) and `tests/setup/.agents/config.yaml`
   (the global config the selected features wrote). Pre-existing-content and
   malformed-block cases are contract tests next to the code, not goldens.
7. **Managed block** — `but` style, own markers, own hardening:
   `<!-- pixel:setup:start -->` / `<!-- pixel:setup:end -->`, line-anchored, CRLF
   preserving, duplicate-converging, refusing a partial block. `pixel uninstall`
   learns to strip it.
8. **Temporary flags** — kebab-case, temporary, documented below. They do not
   ship: the release-prep task deletes them and re-points the golden test at the
   in-process renderer.
9. **Issue** — #890, opened on `Pixel-CLI/pixel` so the PR can `Closes #890`.

## What a feature writes

Each feature is a bundle: a steering section in the managed block, config keys in
`~/.pixel/config.yaml`, and/or a note naming the `pixel install` step that deploys
the matching artifact.

| # | id | label | default | writes |
|---|---|---|---|---|
| 1 | `prompt` | Retrieval-first workflow | on | managed block in the agent's instruction file |
| 2 | `brief` | Per-prompt brief | on | `brief: true` |
| 3 | `guard` | Guard destructive git commands | on | guard hook entry |
| 4 | `metrics` | Command timing and savings | on | `metrics: "on"` + metrics hook entry |
| 5 | `daemon` | Background daemon on demand | on | `daemon_auto_start: true` |
| 6 | `semantic` | Semantic search first | off | steering section |
| 7 | `classify` | AI classification | off | `classify.enabled: true` |
| 8 | `web-search` | Private web-search provider | off | steering section |
| 9 | `rules` | Per-agent rule files | off | per-agent rules file |
| 10 | `pi-extension` | Pi impact extension | on for Pi | the pi extension |
| 11 | `codex-config` | Codex config and hooks | on for Codex | `.codex` config |
| 12 | `land` | Land on the target branch | off, repo-only | steering section |

## Temporary development flags (do not ship)

`but agent setup` has no non-interactive apply, so the port carries three
throwaway flags to drive the wizard from a script while the goldens are built.
They exist for this development loop only.

| flag | meaning |
|---|---|
| `--selected-agents 1,3,4` | answer the agent question with those 1-based indices into the agent list `--help` prints |
| `--selected-features 2,4,12` | answer the feature question with those 1-based indices into the feature list |
| `--dummy-apply` | redirect every write root to `tests/setup/`, so nothing outside the repository is touched |

An index outside the list is an error naming the valid range. `--dummy-apply`
changes no behaviour but the write root.

**Removal gate (before the release PR):** delete the three flags from
`crates/pixel/src/main.rs` and re-point the golden test at the in-process
renderer (`setup::render_block` + `setup::files::upsert`) so the committed
`tests/setup/.agents/` files stay verified without them. `--print` survives as the
permanent non-interactive surface.

## Goal

Import `but agent setup` into pixel as a top-level `pixel setup` command:

1. **detect** which agent CLIs are on this machine / driving this process, with
   the same detection GitButler uses (env-var markers, `$HOME` config dirs,
   unambiguous repo markers, `AGENTS.md` excluded as evidence);
2. ask which of them to set up;
3. ask which **pixel features** to activate (the pixel-native replacement for
   `but`'s `WorkflowOption` list);
4. show exactly what will change — every path and the exact text — and write
   nothing until one confirmation;
5. apply: managed blocks per agent instruction file, per-feature config keys,
   and the per-agent artifacts those features imply.

Explicitly **out of scope** (the `but setup` part of the original): repository
preparation, index building, `pixel prepare-repo`, `pixel build-index`. No
repository state is touched by `pixel setup`.

## Tasks

### 1. Design (before code)

- [x] Answer questions 1–8 above; record the answers at the top of this file.
- [x] Fix the feature → artifact mapping as a table (see above).
- [x] File layout: detection, catalog, planner, renderer and upsert in
      `crates/pixel-install/src/setup/`, command wiring and terminal I/O in
      `crates/pixel/src/setup_cmd.rs`.

### 2. Detection module

- [x] `crates/pixel-install/src/setup/detect.rs`: the env-var detection port
      (`AI_AGENT` → tool-specific markers → allowlisted `AGENT`), the
      `Agent` enum limited to the agents pixel supports, `as_str()` ids used by
      telemetry/action log, and normalization (`Claude_Code`, `claude code`,
      `claude-code@2` → one id).
- [x] `AgentTarget` with `home_marker()`, `repo_config_marker()`,
      `instruction_file(scope)`, and `in_use(home, repo)` — the same three-way
      pre-selection OR as `but`, `AGENTS.md` explicitly not a marker.
- [x] Tests: one per marker and per ordering rule (a fork's marker beats its
      parent's), every agent id parses, an unknown `AI_AGENT` value still counts
      as "an agent is driving".

### 3. Feature catalog

- [x] `Feature` enum with `label()`, `help()`, `default_selected()`,
      `repo_local_only()`, and the artifact mapping; a stable index per feature
      (the index the temporary flags use).
- [x] A table test: every feature id is
      unique, every feature maps to at least one artifact, and no artifact
      appears twice for the same agent.
- [x] Default selection pinned by a test (`render` of the default answers is
      the `pixel setup --print` output).

### 4. Planner and renderer

- [x] `Plan` = scope + selected agents + selected features → concrete writes
      (`config writes`, `agent files`, `notes`), computed before any I/O.
- [x] Renderer: one managed block per agent instruction file, with a baseline
      section plus one `###` section per selected feature; repo-local features
      excluded outside a repo-only setup; per-agent wording where the agent
      differs (e.g. Pi's TS extension vs Claude's Markdown rules).
- [x] Paths derived from one `AgentTarget` table (no
      duplicated path literals), asserted by a test that the wizard's paths
      equal `pixel install`'s.
- [x] Review screen prints every path (`~` collapsed) and the exact text, then a
      single Apply/Cancel. Cancel writes nothing and says so.

### 5. Managed-block upsert

- [x] A dedicated but-style upsert in `setup/files.rs` for the cases
      `but` covers and pixel does not yet: line-anchored markers (a marker in
      prose or inside a fenced block is not a delimiter), CRLF preservation,
      duplicate-block convergence, refusal on partial/reversed markers.
- [x] Atomic write for the wizard path (temp file in the same directory +
      rename), and the concurrent-edit back-off `but` uses.
- [x] Contract tests per property, in the `crates/pixel-install` style
      (`config/contract_tests.rs`).

### 6. The command

- [x] `pixel setup` in `crates/pixel/src/main.rs`, in `--help` order, with
      `#[arg]` docs; `pixel setup --print` non-interactive (plain text and
      `--json`).
- [x] Interactive wizard: a numbered list answered by index/`ask_bool`
      machinery for the features (y/n) and the arrow picker for the agent
      multi-select; `q`/Esc/Ctrl-C cancels without writing.
- [x] Temporary non-interactive flags (`--selected-agents`,
      `--selected-features`, `--dummy-apply`) per answer to question 8; every
      out-of-range index is an error naming the valid range.
- [x] `--dummy-apply`: redirect every write root to `test/.agents/` and write
      there; no other difference in behaviour.
- [x] Errors: no TTY without `--print` → exit 1 pointing at `--print`.

### 7. Tests

- [x] One golden file per agent under `tests/setup/.agents/`, showing the exact
      modification the setup makes for that agent, plus `config.yaml` for the
      global config the selected features wrote.
- [x] CLI tests in `crates/pixel/tests/cli/setup_cli.rs`; the goldens are compared in-process --selected-agents=… --selected-features=…`
      and diffs the result against the committed goldens.
- [x] Tests that a partial selection writes only the selected agents' files and
      only the selected features' sections; that a second run is byte-identical;
      that cancellation writes nothing; that `--print` output matches the
      default render.
- [x] A test that no agent without an instruction file is silently skipped
      (a `manual_instructions_required`-style note, as `but` does).

### 8. Release prep

- [ ] Delete `--selected-agents`, `--selected-features`, `--dummy-apply` and
      re-point the golden test at the in-process renderer (see the removal gate
      above).

### 9. Docs and gates

- [x] `ARCHITECTURE.md` command table (+ the `docs_drift` test if it needs a row),
      `crates/pixel-install/assets/pixel-agent-prompt.md`, `docs/manual-setup.md`,
      README and `website/content/docs.md` if they enumerate commands.
- [x] `changelog.d/pixel-setup-wizard.added.md`.
- [x] `cargo fmt --all -- --check`, `cargo test -p pixel-install`, `cargo test -p pixel-cli --test cli`, `cargo clippy` on both crates, `cargo clippy
      --workspace --all-targets -- -D warnings`.
- [ ] Optional: `pixel-dev build-index --history . && pixel-dev install --repo . &&
      pixel-dev doctor . --fix --fail-on yellow --skip 'install.*'` if the change
      moves what the home install writes.

## Risks

- `pixel install` already owns the agent artifacts. Two writers of the same
  managed block must converge; a divergence here is a data-loss bug for the
  user's own `AGENTS.md` text.
- Adding a top-level `pixel setup` next to `pixel config setup` invites confusion
  and touches the agent prompt and `docs_drift` in both directions.
- The temporary flags are test scaffolding that will ship unless they are hidden
  or gated; the plan keeps them deliberately minimal (three flags).