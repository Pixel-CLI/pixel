# Project Rules

**Before the first edit of any kind, read [CONTRIBUTING.md](.github/CONTRIBUTING.md).**
It defines the build, the gates, the PR format and the definition of done.

Pixel is the deterministic rewrite developed as RTO for coding agents.
The product thesis and every contract live in [`specs/`](specs/); code that
has no spec does not belong here. Read [`specs/thesis.md`](specs/thesis.md)
first.

## Working loop

- One reviewable concern per pull request, branched from the fetched `main`.
- Publish the candidate; CI validates the current head. Local builds, tests
  and review are optional diagnosis, never a ritual.
- Mutation testing runs once a night on `main` only. For each reported
  survivor, add a test that fails under that exact mutation.
- Prototype an uncertain idea in a throwaway script before any Rust
  (`prototype-first`), and measure with the command beside the number
  (`measuring`).
- Code lifted from the previous Pixel engine is copied from the pinned commit,
  then edited in the same pull request. The PR body names each
  `lifted: <source path at pinned commit> → <current path>`.

## Rules directory

Scoped rules live in [`.agents/rules/`](.agents/rules/); `.claude/rules` is a
symlink to it. A tool that does not auto-load them reads the files before its
first edit. Some rules name crates that arrive with later milestones
(`graph-resolver` with `pixel-graph`, `install-layouts` with `pixel-setup`); they
apply from the day that crate exists.

| File | Content |
| --- | --- |
| `rust-style.md`, `test-hygiene.md`, `mutation-gate.md` | the lint idioms, deterministic tests, code that passes mutation on the first run |
| `change-propagation.md` | list every producer and reader of a value before changing it |
| `measuring.md`, `prototype-first.md`, `script-repetitive-work.md` | what a number must carry; prove a bet in a script first; script repetitive work |
| `validation-loop.md`, `review-gate.md`, `test-campaigns.md`, `verify-installed-first.md` | optional local diagnosis, required CI on the current head |
| `architecture-doc.md` | which `ARCHITECTURE.md` section a change owes an edit to |
| `release.md` | releases go through the `release` skill only |
| `project-task.md` | an issue exists before work starts; the PR body opens with `Closes #N` |
| `graph-resolver.md`, `install-layouts.md` | resolution chain and install layouts, once those crates exist |
| `readme-webp.md` | the lossless README animation pipeline |
| `pr-swarm.md` | the pane-per-open-PR reconciler |

## Skills directory

On-demand knowledge lives in [`.agents/skills/`](.agents/skills/) (`.claude/skills`
is a symlink): `rust-guidelines`, `release`, `pr`, `validation-loop`,
`section-redesign`, `agent-session-debugging`, `docker-setup-smoke`,
`improve-codebase-architecture`. When a skill and a rule disagree, the rule
wins; the `Cargo.toml` lint table wins over both.

## Cloud sessions

- Read and write GitHub through the session's GitHub tools, not `gh`.
- Give `rg` an explicit path (`rg -n 'x' .`): with no path it reads stdin.
- One Cargo writer per `target/`; watch the disk.
