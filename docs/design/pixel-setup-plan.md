# `pixel setup` — port of `but agent setup` (plan)

Status: plan, not started. Companion reference: [but-agent-setup-reference.md](but-agent-setup-reference.md)
(what GitButler's wizard does, read in their tree at `5cbe33d` and PR #14425).

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

## Open questions (answer before the first edit)

1. **Agent list.** Which harnesses are offered? Proposal: the ones pixel
   already writes for — Claude Code, Codex, Cursor, Devin, Gemini CLI,
   Antigravity, OpenCode, Pi, GitHub Copilot, Warp — plus a generic
   `AGENTS.md` target. Detection is the port of `detect_agent.rs`, not a new
   heuristic.
2. **Feature list.** Which pixel features are toggleable, and what does each one
   *write*? Candidates already in the tree: metrics footer, daemon auto-start,
   per-prompt brief, classify (+ engine), web-search provider, semantic recall /
   hybrid scope, agent-prompt deployment, guard hook, rules deployment, pi
   extensions, Codex config, Warp MCP. Needs the final list and the per-feature
   artifact mapping (see tasks 3–4).
3. **Feature semantics.** Does a selected feature (a) only write global config
   keys, (b) also drive per-agent artifacts (hooks, prompt, rules), or (c) both?
4. **Scope question.** `but` asks global / repository / both. Keep it? Pixel's
   `install` already has `--repo`; a repo-local setup writes `.claude/…`,
   `.codex/…`, `AGENTS.md` in the repo.
5. **Relationship with `pixel install` and `pixel config setup`.** Does
   `pixel setup` replace `pixel config setup` (deprecate + redirect), or coexist?
   `docs_drift`, `ARCHITECTURE.md`, the agent prompt and `docs/manual-setup.md`
   all have to agree either way.
6. **Golden-file location.** `test/.agents/<agent>.md` as proposed, or
   `tests/.agents/<agent>.md` next to the existing root `tests/` artifacts?
7. **Managed markers.** Reuse the existing `<!-- pixel:managed:begin/end -->`
   block (so `pixel uninstall` keeps working) or a new `pixel:setup` block?
8. **The temporary flags.** Exact spelling/visibility of `--selected-agents`,
   `--selected-features`, `--dummy-apply`: hidden, or gated behind a feature
   flag? Index base: 0- or 1-based, and is the printed list the wizard renders
   (so `--help` shows the index → option mapping)?
9. **Issue.** `project-task.md` requires an issue (project 3). Should I open one
   on `Pixel-CLI/pixel`?

## Tasks

### 1. Design (before code)

- [ ] Answer questions 1–8 above; record the answers at the top of this file.
- [ ] Fix the feature → artifact mapping as a table: feature id, label, help,
      default-on?, what it writes (config key / agent file / hook entry), and
      the agent each artifact applies to.
- [ ] Decide the file layout: which crate owns the wizard, the catalog, the
      renderer and the upsert (proposal: catalog + renderer + upsert in
      `crates/pixel-install/src/setup/`, command wiring and I/O in
      `crates/pixel/src/setup_cmd.rs`).

### 2. Detection module

- [ ] `crates/pixel-install/src/detect_agent.rs`: the env-var detection port
      (`AI_AGENT` → tool-specific markers → allowlisted `AGENT`), the
      `Agent` enum limited to the agents pixel supports, `as_str()` ids used by
      telemetry/action log, and normalization (`Claude_Code`, `claude code`,
      `claude-code@2` → one id).
- [ ] `AgentTarget` with `home_config_marker()`, `repo_config_marker()`,
      `instruction_file(scope)`, and `in_use(home, repo)` — the same three-way
      pre-selection OR as `but`, `AGENTS.md` explicitly not a marker.
- [ ] Tests: one per marker and per ordering rule (a fork's marker beats its
      parent's), every agent id parses, an unknown `AI_AGENT` value still counts
      as "an agent is driving".

### 3. Feature catalog

- [ ] `Feature` enum with `label()`, `help()`, `default_selected()`,
      `repo_local_only()`, and the artifact mapping; a stable index per feature
      (the index the temporary flags use).
- [ ] A `#[cfg_attr(test, mutants::skip)]`-free table test: every feature id is
      unique, every feature maps to at least one artifact, and no artifact
      appears twice for the same agent.
- [ ] Default selection pinned by a test (`render` of the default answers is
      the `pixel setup --print` output).

### 4. Planner and renderer

- [ ] `Plan` = scope + selected agents + selected features → concrete writes
      (`config writes`, `agent files`, `notes`), computed before any I/O.
- [ ] Renderer: one managed block per agent instruction file, with a baseline
      section plus one `###` section per selected feature; repo-local features
      excluded outside a repo-only setup; per-agent wording where the agent
      differs (e.g. Pi's TS extension vs Claude's Markdown rules).
- [ ] Paths derived from the single table `pixel install` already uses (no
      duplicated path literals), asserted by a test that the wizard's paths
      equal `pixel install`'s.
- [ ] Review screen prints every path (`~` collapsed) and the exact text, then a
      single Apply/Cancel. Cancel writes nothing and says so.

### 5. Managed-block upsert

- [ ] Reuse/harden `pixel_install::config::apply_managed_markers` for the cases
      `but` covers and pixel does not yet: line-anchored markers (a marker in
      prose or inside a fenced block is not a delimiter), CRLF preservation,
      duplicate-block convergence, refusal on partial/reversed markers.
- [ ] Atomic write for the wizard path (temp file in the same directory +
      rename), and the concurrent-edit back-off `but` uses.
- [ ] Contract tests per property, in the `crates/pixel-install` style
      (`config/contract_tests.rs`).

### 6. The command

- [ ] `pixel setup` in `crates/pixel/src/main.rs`, in `--help` order, with
      `#[arg]` docs; `pixel setup --print` non-interactive (plain text and
      `--json`).
- [ ] Interactive wizard reusing `config_cmd`'s `KeyReader`/`ask_bool`
      machinery for the features (y/n) and the arrow picker for the agent
      multi-select; `q`/Esc/Ctrl-C cancels without writing.
- [ ] Temporary non-interactive flags (`--selected-agents`,
      `--selected-features`, `--dummy-apply`) per answer to question 8; every
      out-of-range index is an error naming the valid range.
- [ ] `--dummy-apply`: redirect every write root to `test/.agents/` and write
      there; no other difference in behaviour.
- [ ] Errors: no TTY without `--print` → exit 1 pointing at `--print`.

### 7. Tests

- [ ] One golden file per agent under the agreed `.agents/` directory, showing
      the exact modification the setup makes for that agent (fresh file, and
      the pre-existing-content case).
- [ ] A CLI test that runs `pixel setup --dummy-apply --selected-agents=… --selected-features=…`
      and diffs the result against the committed goldens.
- [ ] Tests that a partial selection writes only the selected agents' files and
      only the selected features' sections; that a second run is byte-identical;
      that cancellation writes nothing; that `--print` output matches the
      default render.
- [ ] A test that no agent without an instruction file is silently skipped
      (a `manual_instructions_required`-style note, as `but` does).

### 8. Docs and gates

- [ ] `ARCHITECTURE.md` command table (+ the `docs_drift` test if it needs a row),
      `crates/pixel-install/assets/pixel-agent-prompt.md`, `docs/manual-setup.md`,
      README and `website/content/docs.md` if they enumerate commands.
- [ ] `changelog.d/<slug>.added.md`.
- [ ] `cargo fmt`, `cargo test -p pixel-cli --test cli`, `cargo clippy
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