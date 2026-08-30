# 📋 pixel

> **Deterministic retrieval + git engine for LLM agents.**

If an answer can be retrieved instantly and with certainty from repo state, pixel retrieves it — no grepping, no manual git archaeology, no LLM reasoning about things a query can just answer. The LLM only steps in for genuine ambiguity, like a real merge conflict.

![Status](https://img.shields.io/badge/status-active-success)
![Language](https://img.shields.io/badge/Rust-2024%20edition-orange)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)

## 💡 What this looks like

| The ask | ❌ Without pixel | ✅ With pixel |
|---|---|---|
| "bring back the deleted upload progress code" | walk `git log`/`git show` by hand, risk `git checkout` wiping uncommitted work | `pixel excavate --phrase "upload progress"` → `pixel rescue --apply` *(refuses if dirty)* |
| "I'm getting a 503" | grep the whole tree, guess which hit is real | `pixel resolve "I'm getting a 503"` → exact route, confidence: resolved |
| "sync my branch" | fetch → reason about ahead/behind → merge/rebase → push, by hand | `pixel reconcile` → one call, conflicts get a structured report |
| "add rate limiting to the upload endpoint" | read a dozen files to get oriented | `pixel targets "…"` → closed P0/P1/P2 file list |
| find `handleCheckout` | flat grep, no ranking | `pixel search 'handleCheckout' --scope code` → ranked |
| "can I change `processPayment`?" | grep the name, hope you caught every caller | `pixel impact processPayment --direction upstream` → risk: HIGH, d1/d2/d3 counts |
| "what calls `processPayment`?" | text-match, miss aliased imports | `pixel uses processPayment --role callers` → confidence-tiered |
| "what did my edit touch?" | eyeball `git diff` | `pixel changes` → affected symbols/flows |
| "commit this" | `git add -A` sweeps in unrelated changes | `pixel publish --files checkout.ts --message "…"` → exactly those files |
| "why is there a conflict?" | `git status` can hide conflicted paths | `pixel review` → conflicts always shown |

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
