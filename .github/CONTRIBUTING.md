# Contributing to Pixel

Written to be read by humans and by coding agents. Every rule is a verifiable
command or a checkable invariant. An agent treats "Definition of done" as the
contract for its pull request.

- Product thesis and contracts: [`specs/`](../specs/), start with `thesis.md`
- Architecture map (from M4): `ARCHITECTURE.md`
- Agent rules: [AGENTS.md](../AGENTS.md) and [`.agents/rules/`](../.agents/rules/)
- Security: [SECURITY.md](SECURITY.md)

## Definition of done

Publish a reviewable candidate promptly; required gates run in CI.

- [ ] `cargo fmt --all -- --check` exits 0.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` exits 0.
- [ ] `cargo nextest run --workspace --profile ci` and `cargo test --workspace --doc` exit 0.
- [ ] `cargo deny check` exits 0; a new advisory exception carries its reason and is repeated in `osv-scanner.toml` (`python3 scripts/ci/check-advisory-ignores.py`).
- [ ] New behaviour has a test that fails if the behaviour is removed.
- [ ] Every new source file (`.rs`, `.py`, `.sh`, `.ts`, `.js`, `Cargo.toml`, a workflow) opens with `SPDX-FileCopyrightText: The Pixel contributors` and `SPDX-License-Identifier: MIT`; `python3 scripts/ci/check-spdx.py --fix` adds them. Historical files imported from RTO retain their original attribution.
- [ ] A `.github/changelog.d/<slug>.<section>.md` fragment carries the entry (skip for pure refactors, CI/deps chores and website-only changes), opening on its scope (`**graph:** …`), under 500 bytes.
- [ ] Conventional Commits; branch from an up-to-date `main`; the PR targets `main`.
- [ ] The change implements a section of a spec in `specs/` and the PR body cites it. A behaviour change edits the spec in the same PR.
- [ ] Lifted code: the PR body lists `lifted: <source path at pinned commit> → <current path>` and what changed, and the lifted tests pass here.
- [ ] `ARCHITECTURE.md` matches the change (`.agents/rules/architecture-doc.md`).
- [ ] A hook, an install target, a file under `.pixel/` or `~/.pixel/`, a network destination or a secret changes `specs/state-and-threats.md` in the same PR.
- [ ] The work has an issue, and the PR body opens with `Closes #<number>` (`Refs #<number>` for part of one).

## Prerequisites

Stable Rust with `rustfmt` and `clippy`; `git`; `cargo-nextest` (`cargo install --locked cargo-nextest`);
`bun` for `docs/motion`; `hugo` (version pinned in `website/mise.toml`) for the site.

## Build and test

```bash
cargo build --release -p pixel
cargo nextest run --workspace
cd docs/motion && bun install --frozen-lockfile && bunx @typescript/native-preview --noEmit -p .
cd website && mise exec -- hugo --gc --minify
```

One Cargo writer per `target/`; `df -h .` before long campaigns.

## Commits and pull requests

Conventional Commits (`feat(graph): …`, `fix(hooks): …`). A pull request is one
reviewable concern under about 400 changed lines, excluding lifted files, with
the sections of `.github/pull_request_template.md`: Summary, Evidence
(before/after with commands), Merge Danger (door and blast radius), Docs touched.

## Mutation testing

Nightly on `main` only (from M2). Never local, never a PR gate. Read the
report and add a test that fails under each surviving mutant
(`.agents/rules/mutation-gate.md`).

## Releases

Through the `release` skill only (`.agents/rules/release.md`).
