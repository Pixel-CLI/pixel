# 📋 pixel

> **Deterministic retrieval + git engine for LLM agents.**

If an answer can be retrieved instantly and with certainty from repo state, pixel retrieves it — no grepping, no manual git archaeology, no LLM reasoning about things a query can just answer. The LLM only steps in for genuine ambiguity, like a real merge conflict.

![Status](https://img.shields.io/badge/status-active-success)
![Language](https://img.shields.io/badge/Rust-2024%20edition-orange)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)

## 💡 What this looks like

Numbers are illustrative (order-of-magnitude), not a benchmark — the source diagrams live in `docs/examples/` and get regenerated once real measurements replace them.

### Recovering deleted code

![Recovering deleted code — without vs with pixel](docs/examples/01-recover-deleted-code.svg)

### Locating code by an error

![Locating code by an error — without vs with pixel](docs/examples/02-resolve-error.svg)

### Syncing a branch

![Syncing a branch — without vs with pixel](docs/examples/03-reconcile-branch.svg)

### Starting a task

![Starting a task — without vs with pixel](docs/examples/04-targets-task.svg)

### Searching the codebase

![Searching the codebase — without vs with pixel](docs/examples/05-search-ranked.svg)

### Blast radius before editing

![Blast radius before editing — without vs with pixel](docs/examples/06-impact-blast-radius.svg)

### Finding real callers

![Finding real callers — without vs with pixel](docs/examples/07-uses-callers.svg)

### Checking impact before committing

![Checking impact before committing — without vs with pixel](docs/examples/08-changes-precommit.svg)

### Committing a change

![Committing a change — without vs with pixel](docs/examples/09-publish-commit.svg)

### Reviewing a conflict

![Reviewing a conflict — without vs with pixel](docs/examples/10-review-conflicts.svg)

## ✨ Features

- **Search** — indexed, git-anchored, ranked. `search --scope code`.
- **Code graph** — tree-sitter across TS/TSX/JS/Rust/Go/Java/Python: `symbol`, `impact`, `uses`, `trace`, `changes`, token-budgeted `context`.
- **Resolve** — a phrase, label, or error → exact code, with an honest confidence level (`resolved` / `ranked` / `unresolved`).
- **Excavate + rescue** — finds code no longer at HEAD (deleted, stashed, on another branch) and restores it safely, refusing dirty files without a strategy.
- **Reconcile** — one-call branch sync: fetch, classify, act; real conflicts get a structured report, not silence.
- **Git mutations** — `publish`/`push`/`branch`/`update` etc., snapshot-token gated and crash-safe.
- **Ranking signals** — recency and live session context rerank results, never promoting a stale file above a better match.
- **Daemon** — a warm background process keeps everything sub-millisecond; falls back to in-process automatically.

## 🔧 Install

```bash
git clone https://github.com/LivioGama/pixel.git
cd pixel && cargo build --release
pixel install   # registers hooks + agent-config rules
pixel doctor
```

No indexing step needed — it builds lazily on first use.

## 📖 Agent Rules

Mandatory usage rules: [`~/.agent-config/rules/pixel.md`](https://github.com/LivioGama/pixel-rules) and `~/.agent-config/skills/pixel/SKILL.md`.

## 📄 License

MIT (derived code from hypergrep, MIT; ClickHouse sparse-grams algorithm, Apache-2.0 — see NOTICE).
