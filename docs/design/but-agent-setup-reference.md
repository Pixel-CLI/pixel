# `but agent setup` — GitButler's agent-onboarding wizard (reference)

Status: reference note. Read before designing `pixel setup`. Every claim below
was read in the GitButler tree at `5cbe33d` (main, `crates/but` only) and in
PR [#14425 "Add `but agent setup` wizard"](https://github.com/gitbutlerapp/gitbutler/pull/14425)
(merged 2026-06-24), plus the published pages
[`ai-agents/getting-started`](https://docs.gitbutler.com/ai-agents/getting-started)
and [`ai-agents/tuning-agent-behavior`](https://docs.gitbutler.com/ai-agents/tuning-agent-behavior).
Nothing in this file is a pixel decision; the port plan is
[pixel-setup-plan.md](pixel-setup-plan.md).

## What it is

`but agent setup` is a **pure file-writing TUI wizard** that

1. installs GitButler's embedded *agent skill* (four Markdown files) into each
   selected agent's native `skills/` directory, and
2. upserts a **managed Markdown block** of workflow steering into that agent's
   instruction file (`AGENTS.md`, `CLAUDE.md`,
   `.github/copilot-instructions.md`, …),

then optionally runs `but setup` to put the repository into GitButler workspace
mode. That last step is explicitly **not** part of the pixel port.

It has no hooks, no MCP, no daemon, no provenance pinning, no uninstall command
and no verification step: it is documentation distribution.

## Code map (GitButler, `crates/but`)

| File | LOC | Role |
| --- | --- | --- |
| `src/args/agent.rs` | 40 | clap surface: bare `but agent` == `but agent setup`; flags `--print`, hidden `--stub` |
| `src/command/agent/mod.rs` | 809 | wizard flow, prompts, review screen, apply |
| `src/command/agent/plan.rs` | 357 | agent targets → concrete paths (the "planner") |
| `src/command/agent/policy.rs` | 319 | renders the Markdown steering from the answers |
| `src/command/agent/files.rs` | 168 | managed-block upsert primitive |
| `src/command/agent/cleanup.rs` | 343 | retired-syntax sweep run on **every** command |
| `src/command/agent/tests.rs` | 712 | 42 unit tests |
| `src/command/skill/mod.rs` | 74 KB | `SKILL_FORMATS` = single source of truth for install paths; `write_skill_files` |
| `src/command/skill/freshness.rs` | ~9.5 KB | skill self-heal / "AGENT ACTION REQUIRED" notices |
| `src/utils/detect_agent.rs` | 459 (+67 tests) | 37-agent env-var detection |
| `skill/{SKILL.md, references/*.md, stub.md}` | 2040 | the embedded bundle |

## The flow (4–5 screens, `command/agent/mod.rs:66-800`)

```text
GitButler · agent setup
────────────────────────
• Install the GitButler skill so your agent can drive but
• Save a few preferences for how it commits, branches, and opens PRs
"Nothing is written until you review and confirm — you'll see exactly what changes first."
↑/↓ move · space select · enter continue · esc cancel

Step 1 of 4  Agents          multi-select, pre-checked from detection
Step 2 of 4  Where it applies single-select (skipped outside a repo → Global forced)
Step 3 of 4  Preferences     multi-select + sub-prompts for free text
       → sub-prompts         publish phrase / branch pattern / commit convention
Step 4 of 4  Review          exact paths + exact generated text
       → Apply / Cancel      one confirmation
```

- **Everything is collected into a pure `Plan` first** (`mod.rs:116`,
  `plan.rs:215`); `apply_plan` (`mod.rs:731`) only writes.
- **Cancelling never writes.** A dedicated `UserCancelled` error is threaded
  through every prompt; `Esc`/`Ctrl-C` on any screen prints "Cancelled. No
  skill was installed and no agent files were modified."
- Banner and step counter are computed from terminal width clamped to 40–72
  columns; the product name is shown once, in the intro.
- **Apply order is deliberate:** `but setup` first (the failure-prone step, so
  the abort happens before any write), then skill writes, then instruction
  writes. The remaining writes are idempotent upserts, so a partial run is
  re-runnable.
- **`--print`** is the only non-interactive path: it renders
  `WizardAnswers::default()` to stdout, or `{"policy": "…"}` under `--json`.
  Without a TTY, interactive mode bails with a pointer to `--print`.

## Agent detection and pre-selection

Pre-selection is a three-way OR (`plan.rs:114-164`, `utils/detect_agent.rs`):

1. **The agent currently driving the CLI**, from env vars: the generic
   `AI_AGENT` convention (`@vercel/detect-agent`) first, then ~40
   tool-specific variables (`CLAUDECODE`, `CURSOR_AGENT`, `CODEX_SANDBOX`,
   `PI_CODING_AGENT`, `DSH_SHELL`, …), then `AGENT` last with a **strict
   allowlist**. 37 `Agent` variants; the order is documented as a contract
   (Kilo is an OpenCode fork so its marker must win; `AGENT` is generic so it
   loses to any fresh tool-specific marker).
2. **Config dir under `$HOME`**: `~/.codex`, `~/.claude`, `~/.cursor`,
   `~/.copilot`, `~/.codeium`, `~/.config/opencode`, `~/.config/poolside`.
3. **Unambiguous repo marker**: `CLAUDE.md`, `.cursor/`, `.poolside/`,
   `.github/copilot-instructions.md`.

`AGENTS.md` is *explicitly excluded* as a marker — six of the eight targets
share it, so it is never evidence for a specific one. Detected rows are
labelled `"(detected)"`. Emptying the selection is treated as intentional
(defaults are cleared) but rejected, so the user cannot continue with nothing.

**Coverage gap:** the wizard offers **8** targets, while `SKILL_FORMATS` knows
~25 formats and `detect_agent` knows 37 agents. `but skill install` supports
far more agents than `but agent setup` does.

## What gets written

### Skill bundle (`command/skill/mod.rs`)

Embedded with `include_bytes!` at compile time:

```text
SKILL.md                    (225 lines; YAML frontmatter: name/version/description/author)
references/reference.md     (718) — served content generated from the clap tree
references/concepts.md      (351)
references/examples.md      (498)
```

- **Version stamping:** `version: 0.0.0` in the frontmatter is replaced with
  `option_env!("VERSION")` at write time; `but skill check` compares the
  installed version with the CLI version.
- **Write order:** references first, `SKILL.md` **last**, so a crashed write
  never leaves a bundle that looks complete (asserted by a test).
- **Identity is content-based:** discovery scans any folder under
  `<agent>/skills/` and requires `name: but` in the frontmatter.
- **Stub layout** (hidden `--stub`): one `SKILL.md` carrying the same
  frontmatter plus `stub: true` and `allowed-tools: Bash(but skill:*)`, whose
  body tells the agent to run `but skill` ("about 200 lines, other commands in
  the same call get buried"). The pre-approval line exists so the redirect
  never waits on a permission prompt.
- `references/reference.md` has a **dual source of truth**: the installed file
  is hand-written, but `but skill reference` *prints* a version rendered from
  the clap tree, so the served text cannot drift from the binary.

### Managed steering block (`policy.rs:146+`, `files.rs`)

```markdown
<!-- gitbutler-agent-setup:start -->
## Version control
- Use GitButler (`but`) for version-control inspection and write operations…
- Assume multiple agents may be working in this repository. Do not modify another agent's work…
<!-- gitbutler-agent-setup:end -->
```

- An **always-on `## Version control` baseline** (9 bullets) is emitted
  regardless of the answers; the ten `WorkflowOption`s each contribute an
  optional `###` section. The text mirrors the published docs page, so the
  wizard output is close to hand-copying the documented snippets.
- Defaults: *amend local fixes* + *suggest splits*. *"Push to target"*
  (skip-the-PR / land on main) is `repo_local_only()` and is rendered
  **disabled** outside a repo-only setup, with help text explaining that a
  repo-local rule must not leak into global config. The row label stays
  identical across scopes; dimming and help carry the meaning.
- The policy is rendered **once** and written to every target file — which is
  exactly why repo-local options are excluded at the scope level.

The upsert primitive (`files.rs`) is the most defensive code in the feature:

| Property | Implementation |
| --- | --- |
| Idempotency | markers found → first block replaced, extra blocks dropped (converge to one) |
| Refuses malformed state | start-without-end, end-without-start, end-before-start → error, no write |
| Markers must be line-anchored | a marker quoted in prose or inside a fenced code block is not a delimiter (`find_line_anchored` + `inside_fenced_block`) |
| CRLF | the replaced/appended block is normalized to the file's existing endings — no mixed EOL |
| Blank-line separation | append adds the missing blank line (LF and CRLF variants) |
| Parent dirs | created on demand |

## The part that matters more than the wizard: the runtime loop

`lib.rs:557-615` — before every **non-interactive** command (excluding `skill`,
`agent`, `mcp`, `help`, `completions`, `metrics`, `external`), when an agent is
detected:

1. **Skill freshness** (`skill/freshness.rs`)
   - not installed → `⚠ AGENT ACTION REQUIRED: … run: but skill install …
     This notice repeats until the skill is installed. If it still appears
     after installing, report it instead of retrying.` (a unit test pins the
     repeat-on-purpose wording as contract);
   - stale → **silently rewrite** every outdated install, then report the
     version it moved to;
   - update failure → notice includes the error plus `but skill check --update`;
   - killed by a user setting `agent_skill_notices`.
2. **Retired-policy sweep** (`command/agent/cleanup.rs:39`) — the most
   interesting technique in the feature.

### The retired-syntax sweep

Problem: the wizard's answers are **not persisted anywhere**, so a wording
change to a machine-written bullet cannot be re-rendered for existing installs.
Fix: an exact-string rewrite of the retired bullet, inside the managed markers
only, run on every command.

What makes it safe to run implicitly:

- runs only when an agent is detected; `catch_unwind` wraps the whole sweep, so
  it "cannot take down the command that triggered it";
- **global files only** (`~/.codex/AGENTS.md`, `~/.claude/rules/gitbutler.md`,
  …), explicitly **not** repo-local `AGENTS.md`/`CLAUDE.md`: those are usually
  git-tracked and this runs right before a command, so dirtying them could get
  swept into an unrelated commit;
- cheap `contains()` check first; malformed block → skip; non-UTF-8 → skip;
  >1 MB → skip; not a regular file → skip;
- **atomic**: `NamedTempFile` in the same directory → restore the original
  permissions → `sync_all` → **re-read and compare with the read snapshot**
  (bail "changed while preparing the rewrite") → `persist()`. Two concurrent
  invocations converge;
- **symlinks** are resolved only on the rare rewrite path, so a dotfiles-managed
  rules file keeps its link instead of being replaced by a regular file;
- the replacement text is derived from the constant the wizard renders, so the
  two cannot drift;
- the notice **restates the rule in full**, because the agent's context already
  holds the stale text loaded at session start.

### Telemetry

PostHog: `agentSetupOutcome` ∈ {`printOnly`, `cancelled`, `completed`},
`agentSetupManualInstructionsRequired`, `agentSkillHintShown`,
`retiredPolicySyntaxCleaned`, `retiredCommitSyntax`, plus an `agent` property on
**every** event, from the same detection module.

## Testing strategy

- **42 unit tests** over pure functions (`render_managed_policy_block`,
  `collect_skill_installs`, `collect_instruction_writes`, `display_path`,
  `AgentTarget::from_detected`, `upsert_managed_block`). They pin *semantics*,
  not snapshots: `push_to_target_supersedes_publish_phrase_pull_requests`,
  `default_policy_omits_land_section_until_selected`,
  `skill_installs_copilot_repo_and_global_diverge`,
  `every_agent_resolves_to_a_skill_path_for_both_scopes`.
- **8 tests** on the cleanup path: symlink preservation, permission
  preservation, concurrent-edit back-off, CRLF, oversized/binary skip.
- **67 tests** in `detect_agent/tests.rs`.
- **3 CLI integration tests**: `--print` stdout, `--json` shape, no-TTY failure
  with exact stderr. The TUI wizard itself is not snapshot-tested at CLI level —
  it stays testable by splitting prompts from planning from writing.

## Assessment

**Strong**

- The plan/review/apply split is the right shape for a destructive installer:
  nothing touches disk until the user has seen the exact text.
- Install paths are derived from the single `SKILL_FORMATS` table through
  `skill::path_components_for(name, global)`, so the wizard installs exactly
  where `but skill check` discovers.
- The managed-block upsert is hardened against every realistic corruption mode.
- The runtime self-heal loop (auto-update a stale skill, rewrite retired
  syntax, repeat-until-fixed notice) turns a one-shot installer into a maintained
  one, and it is instrumented.

**Gaps / rough edges**

- **Answers are never persisted.** Re-running asks everything again, and any
  wording change forces the ad-hoc retired-bullet sweep.
- **No non-interactive apply.** No `--agents`, `--scope`, `--preferences`,
  `--yes`: an agent or CI cannot install the steering unattended.
- **No uninstall / revert.** The block can only be removed by hand.
- **Coverage mismatch:** 8 wizard targets vs 25 skill formats vs 37 detectable
  agents; the rest map to `None` and vanish.
- **Writes are non-atomic** on the main path (`std::fs::write`), while the
  cleanup path gets tempfile + rename + compare. The stricter discipline sits
  on the rarer path.
- Cursor and the shared "Agent Skills" format have **no global instruction
  file** → a manual-copy note and `manual_instructions_required: true` in the
  outcome.

**Takeaway.** GitButler solved agent onboarding as *documentation distribution*:
a TUI that renders documented policy into per-agent Markdown files inside
idempotent managed blocks, plus a lightweight runtime that keeps those files and
the installed skill bundle fresh. It deliberately avoids hooks, MCP and
installed-code trust, trading integration depth for near-zero blast radius.