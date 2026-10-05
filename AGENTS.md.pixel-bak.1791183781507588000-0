# Project Rules

**Before the first edit of any kind, read [CONTRIBUTING.md](CONTRIBUTING.md).** It defines the build, gates, PR format and definition of done; the loop below is the per-turn addendum.

## Mutation Testing Loop

- Mutation testing runs twice, both off the laptop: the pre-push gate (`scripts/mutants-remote-gate.sh`) bundles the committed diff to the gate host (default the ssh alias `a2`) and blocks the push on the campaign's verdict against a warm `target/`; the `Mutants` workflow then re-runs the same diff's shards on the same host and fails the pull request on any surviving mutant.
- Do not run `cargo mutants` locally on your own initiative; it holds the tree (`--in-place`) and a laptop for up to hours, which is what the gate host and the workflow's runners are for.
- The loop is: write the code in the shapes `.agents/rules/mutation-gate.md` describes, pass the fast gates (`cargo fmt`, `cargo test`, `cargo clippy`), push. A blocked push already lists the `MISSED`/`TIMEOUT` lines from the gate host — fix them and push again; the traveling outcome cache (`target/mutants-preflight/`) makes the retry re-test only the survivors. A PR opened after a green push has `Mutants in diff` necessarily green.
- A local `cargo mutants … -F '<fn>'` on one or two functions, bounded to a few minutes, is acceptable only when explicitly asked for.
- When the gate host is down: `PIXEL_MUTANTS_GATE=off git push` skips the remote run and leaves the verdict to CI. Do not make that the habit — CI then waits 5 to 10 minutes to say what the gate would have said in one.

One habit keeps the CI side from being the bottleneck:

- **Do not wait on the job.** Start `gh pr checks <pr> --watch` as a background task and work on the next unit (the next pull request of the stack, another worktree) until it returns; then read the `MISSED` lines. Never a foreground `sleep` loop.

For each `MISSED` line either:

- add a test that fails under that exact mutation (an assertion on the observable contract, not a weaker one), or
- when the mutation cannot matter (a diagnostic formatter, a `main`, dead-by-design code), annotate the function with `#[cfg_attr(test, mutants::skip)]` plus a one-line reason, adding `mutants = { workspace = true }` to that crate's `[dependencies]` if it is the crate's first skip.

Push the fix and let the workflow re-run until it reports `0 missed`. Skipping a business rule because the test is hard is not an option; see CONTRIBUTING.md "Mutation testing" for the outcome table and exit codes.


## Rules Directory

Scoped rules live in [`.agents/rules/`](.agents/rules/), one Markdown file
per concern, with an optional `paths:` front matter naming the globs they
apply to:

| File | Applies to | Content |
| --- | --- | --- |
| `mutation-gate.md` | `crates/**/*.rs` | writing code and tests that pass `cargo mutants` on the first run |
| `test-hygiene.md` | `crates/**/*.rs` | git fixtures, env vars, canonical paths, assertions as strong as the contract (every form of a "must not happen", values over non-emptiness, the production path, one case per consuming path) |
| `test-campaigns.md` | always | running long mutants/nextest campaigns without surprises |
| `measuring.md` | always | what a number must carry before it is evidence: run identity, the command beside the count, one variable per ablation, the baseline |
| `rust-style.md` | `crates/**/*.rs` | the shapes the four pedantic lints expect (`uninlined_format_args`, `map_unwrap_or`, `redundant_closure_for_method_calls`, `items_after_statements`) and the cleanup after `clippy --fix` |
| `release.md` | always | releases go through the `release` skill only: a prepare PR into `main`, then a tag on `main` |
| `architecture-doc.md` | always | which `ARCHITECTURE.md` section a change owes an edit to, and what `docs_drift` already enforces (command order, crate table and dependencies) |
| `change-propagation.md` | always | list every producer and reader of a value before changing it; siblings without the new input, version bumps, named constants, secrets in every sink, timers around the real cost |
| `graph-resolver.md` | `crates/pixel-graph/**` | the chain a resolution change crosses (extraction, storage, index, six resolution paths, `rename`, diagnostics), the language rule it models, tier honesty |
| `install-layouts.md` | `crates/pixel-install/**` | repo equal to `$HOME`, foreign configs, quoted paths in pasted commands, global and repo-local state kept apart |
| `verify-installed-first.md` | always | when a change moves installed behaviour (hooks, deployed prompts, the binary), install and verify against the real installed binary / a real hook payload *before* writing or churning unit tests — a hand-built unit call that passes while the installed path is broken proves nothing |
| `readme-webp.md` | `docs/examples/*.webp`, `docs/motion/**` | the verified lossless pipeline for README animated webp: render crf=10, 1600×1000 lanczos frames, `img2webp -lossless`, embed `width="800"` |
| `project-task.md` | always | before any work: find the issue on [project 3, view 1](https://github.com/users/LivioGama/projects/3/views/1) or open one and add it; the PR body opens with `Task <number>` (declared exceptions: `no task: <reason>`); the board Status follows the PR — In Progress at open, Done only at merge, back to Todo when closed unmerged |
| `review-gate.md` | always | before pushing a feature branch: fetch the remote default (rebase only on a conflict or a needed change), then fix every `pixel review-gate` finding at CONCERN or above — the pre-push hook enforces the review |
| `validation-loop.md` | always | publish one current validated candidate: scoped local feedback while editing, a frozen full-gate result, baseline compile and remote mutation verdict before Rust pushes, and precise CI triage |
| `pr-swarm.md` | `scripts/pr-swarm.sh`, `.claude/settings.json` | the rmux pane-per-open-PR reconciler: the tool, the SessionStart watcher that replaces launchd (macOS TCC denies launchd any path under `~/Documents`), and the teardown rails that keep a merged PR's worktree when it is dirty, unpushed or the shared cache |

`.claude/rules` is a symlink to that directory (Claude Code loads it by
itself, honouring `paths:`). A
tool that does not auto-load a rules directory (Codex, pi, Devin) reads the
files listed above before its first edit; the `paths:` front matter tells it
which ones matter for the files it is about to touch. Add a rule as a new
file here, never as a second copy in a tool-specific directory.

## Skills Directory

On-demand knowledge lives in [`.agents/skills/`](.agents/skills/), one
directory per skill with a `SKILL.md` (front matter `name` + `description`)
and its supporting files; `.claude/skills` is a symlink to it.

| Skill | Load when | Content |
| --- | --- | --- |
| `rust-guidelines/` | writing, refactoring or reviewing anything under `crates/` | Microsoft's Pragmatic Rust Guidelines (`M-*` ids): a workspace-specific checklist in `SKILL.md`, the full MIT-licensed text in `guidelines.txt` to grep by id, never to read whole |
| `release/` | cutting a release or hotfix, bumping the version, tagging, or a failed Release run | the tag-to-tap procedure around `.github/workflows/release.yml`, and `prepare.sh`, which bumps every member, cuts the changelog, refreshes `Cargo.lock` and runs `check-release` |
| `pixel-retro/` | asking what to improve in pixel from recent usage, "/pixel-retro", or where agents wait in this repo's development loop | mines a window (24 h by default) of every repo's `.pixel/actions.jsonl` and the `pixel recall` transcripts for frictions pixel caused, reproduces each on the current binary, and reports ranked, evidence-backed suggestions without implementing any; `lead_time.py` measures this repo's pull requests from first edit to validated CI |
| `docker-setup-smoke/` | replaying a new user's setup in Docker: install, personal settings kept, reinstall, first audit, doctor, uninstall | tests a release archive, install.sh, the Homebrew tap or compiled main/PR/commit sources in a disposable Linux container with pre-existing Claude/Codex/pi settings; `--agents` adds Claude Code, Codex and pi sessions against a scripted fake model; no real LLM, host configuration untouched, saved provenance, reports and exit status |
| `section-redesign/` | reworking a section of the pixel-cli.dev home, "/section-redesign #<anchor>", "même méthode que le hero" | the loop the hero went through: visitor critique, an artifact of mocked variants with votable points (`assets/review-sheet.html`), synthesis iterations, every claim checked against code and benchmarks, the Hugo implementation with desktop and mobile captures; lists the decisions that already bind every section |
| `improve-codebase-architecture/` | manual only: "/improve-codebase-architecture [area]", an architecture review, where to deepen modules | finds deepening opportunities from churn, `pixel audit`, areas and callers, writes a local HTML report of before/after cards with their measured cost (mutants, diff, `ARCHITECTURE.md` sections, impact risk), then grills the picked candidate into a project-3 task; settled decisions live in `ARCHITECTURE.md` and `.agents/rules/`; adapted from Matt Pocock's MIT skill (`UPSTREAM` pins the commit) |
| `agent-session-debugging/` | debugging real pi, agy, Claude, or Codex behavior in Herdr, especially Pixel retrieval, metrics, or installed hooks | keeps the main operator pane beside a 2×2 agent grid; inspect real transcripts, ask the CLI directly when its UI is ambiguous, then implement, retest, and redeploy with evidence |
| `validation-loop/` | implementing Rust, preparing a PR, or reducing compile and mutation-fix round trips | chooses scoped local feedback, freezes one candidate, uses the remote mutation gate, and triages failed gates without blind push retries |

Rules are always-on for the files they name; a skill is read when its
`description` matches the task. A tool without skill support reads
`.agents/skills/<name>/SKILL.md` before its first Rust edit. When a skill and
a rule disagree, the rule wins (and the `Cargo.toml` lint table wins over both).

## Local Validation Loop

- During editing, run the tests for the changed contract and its affected consumers, plus crate-scoped compilation or Clippy when needed. Run the full gates in CONTRIBUTING.md once the reviewable unit is ready, and again after a fix that can change their result; a status update or an unchanged tree does not need another run.
- Long local gates may run as a background task. Keep their checkout unchanged until they finish; to keep editing, validate a committed snapshot in a separate worktree with its own `target/`. Record the SHA, command, log and exit status. A pass covers that snapshot only, not later edits.
- Keep Cargo builds sequential within one `target/`; parallel workers need separate build directories and a combined CPU/memory budget. Do independent review or another unit while gates run. When no useful independent work remains, wait for completion without repeated polling.
- The CI lanes remain required before declaring the PR ready. See CONTRIBUTING.md "Agent validation workflow" for the local/CI split and how to verify the final run.

## Reinstall and Reconfig After Each Implementation Unit

Complete this checklist once per finished reviewable implementation unit—including tests, website behavior/assets, install/config, and other non-Rust code—before declaring it done. Intermediate edits and progress replies do not trigger it. Run it earlier when a check needs the installed binary or hooks to exercise the new behavior, and repeat it if subsequent edits change the binary or installed rules. Only the explicit skip cases below apply:

1. **Rebuild and reinstall the pixel binary** so the installed CLI matches the working tree:
   ```bash
   pixel self-update --repo . --build "cargo build --profile dev-release -p pixel-cli"
   ```
   `dev-release` is the release profile without thin LTO and with 16 codegen units: an incremental rebuild takes seconds instead of a minute, and the binary is optimised the same way. Drop `--build` only when you need the exact shipped `release` profile. It runs that build, installs over the binary that is actually running (`pixel` resolved through any shim; `~/.local/bin/pixel` only as a last resort — never copy there by hand, a second copy shadows the managed one), stops this repo's daemon, and warns if another `pixel` earlier on PATH would still be picked up. When that binary belongs to mise (`~/.local/share/mise/installs/`) or Homebrew (a Cellar), it refuses and writes nothing: pass `--dev` from the start (`command -v pixel` under `mise/` or a Cellar tells you before the build) and follow "Side build" below, or pass `--install-path` to overwrite the managed binary on purpose. The install is an atomic rename: in-place `cp` over a mapped Mach-O invalidates the ad-hoc signature on macOS and SIGKILLs the next invocation.
2. **In parallel** (both only need the new binary, not each other):
   - **Track A:** `pixel build-index --history .` — rebuild the facts/history index.
   - **Track B:** `pixel install` — reinstall hooks and managed blocks. Where `build-agent-config` is installed (it regenerates per-tool rule directories from `~/.agent-config`), run it first: `build-agent-config && pixel install`.
3. **Run `pixel doctor . --fix --fail-on yellow`** and confirm it exits 0: `--fix` runs each repair command once and re-runs the checks (explicitly report each repair that did not end `fixed`, and each check left with a `fix:` line it cannot run by itself).

Do not report the unit complete without evidence that self-update succeeded, both parallel tracks completed, and doctor exited 0. If a step cannot run, report the unit as incomplete and name the blocker rather than silently skipping it.

One check needs a human and no `--fix` clears it: `repo.codex-hook-review`. Codex runs a repository's hooks only after someone reviews them with `/hooks` in `codex`, and it keys that review by the `hooks.json` path, so every new worktree starts yellow; pixel never writes that trust itself. In a worktree Codex will not run in, add `--skip repo.codex-hook-review` to the doctor command (here and in the side build below) and name the skip in the report. Where Codex will run, review the hooks once with `/hooks` instead.

### Side build (`pixel-dev`)

With a mise or Homebrew `pixel`, the home install (the Claude hooks in `~/.claude/settings.json`, the deployed prompts, the Codex and pi config) belongs to that managed binary and every other repository runs it. A global `pixel-dev install` rewires all of it to a branch build until someone runs `pixel install` again, so the side build stays in this repository:

1. `pixel self-update --dev --repo . --build "cargo build --profile dev-release -p pixel-cli"`.
2. Track A: `pixel-dev build-index --history .`. Track B: `pixel-dev install --repo .` (the repo-local files only).
3. `pixel-dev doctor . --fix --fail-on yellow --skip 'install.*'`: the `install.*` checks judge the home install, which is the managed binary's. A side build's `--fix` never runs a home-install repair (it prints `left pixel install … to the managed pixel`): a `rule.*` check still flagged after it means this build's CLI no longer accepts the deployed rules, a change to the home install.

Only when the unit changes what the home install writes (`crates/pixel-install/`, the prompts in `crates/pixel-install/assets/` or `rules/`, a hook) does the global install belong to the check: run `pixel-dev install` and the full `pixel-dev doctor . --fix --fail-on yellow`, then hand the machine back with `pixel install` and `pixel doctor . --fail-on yellow` through the managed binary, and report both. Using `pixel-dev` is expected here: do not narrate it.

Both commands target the account's login shell (from the user database, not `$SHELL`, which an agent's command tool overrides: Claude Code's runs under `/bin/zsh` on a fish machine): `install` removes the retired `claude()` wrapper from that shell's profile and `doctor` reports one that remains. If `doctor` still reports `install.legacy-wrappers` for the wrong profile, pass the shell a human launches `claude` from to both commands: `--shell fish`.

### When to skip

- Pure read-only exploration (no edits to `crates/` or rules).
- The turn only touched docs, prompts, bench scripts, or contributor instructions (`AGENTS.md`, `CONTRIBUTING.md`, `.agents/`) — nothing that changes binary behavior or installed rules.
<!-- pixel:warp-retrieval:begin -->
This repository has a Pixel index (`.pixel/`). Retrieval starts with Pixel: `pixel search-content -F '<identifier>'` for exact identifiers, `pixel find-code '<concept>'` for behavior-described code, and `pixel impact '<symbol>'` before renames — a native grep/rg over indexed code is a missed retrieval; native tools stay available for everything Pixel does not cover, and two fruitless pixel calls mean switch to grep.
<!-- pixel:warp-retrieval:end -->
