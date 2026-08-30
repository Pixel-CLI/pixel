# 📋 pixel

> **Deterministic retrieval + git engine for LLM agents.**

A Rust sidecar and MCP server that replaces grep-style scanning, raw git archaeology, and manual branch-sync reasoning with deterministic, sub-millisecond operations. The doctrine: if an answer can be retrieved with 0ms and perfect certainty from repo state, it must be — never reasoned about, never searched for by hand. The LLM only gets involved for genuine ambiguity (a real merge conflict, an unresolved phrase).

Supersedes and replaces **[gitpixel](https://github.com/LivioGama/gitpixel)** and **usable-git** — both retired.

![Status](https://img.shields.io/badge/status-active-success)
![Type](https://img.shields.io/badge/type-tool-blue)
![Language](https://img.shields.io/badge/Rust-2024%20edition-orange)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)

## ✨ Features

🔍 **Indexed regex search** — Trigram shard (mmapped, delta-varint postings), regex→boolean query planning, candidate verification. `search --scope code` reranks by filename/symbol/content-density signals without changing the hit set.

⏱️ **Git-anchored freshness** — Base shard pinned to a commit OID, a committed-delta layer on HEAD moves, and an in-memory dirty overlay for uncommitted edits fed by an fs watcher.

🕸️ **Code graph** — Tree-sitter extraction (TS/TSX/JS, Rust, Go, Java, Python) into SQLite; tiered call resolution that never fans out ambiguous names into edges. `impact`, `uses`, `trace`, `processes`, `clusters`, `changes`, token-budgeted `context`.

🧭 **Concept resolution (`resolve`)** — Extracts UI labels, JSX text/attrs, component names, forms, routes, and HTTP status codes into a concept index. `pixel resolve "the form"` or `pixel resolve "I'm getting a 503"` resolves a bare phrase to exact code via a T0-exact-unique → T1-kind-directed → T2-word-intersection → T3-trigram cascade, with an explicit confidence level (`resolved` / `ranked` / `unresolved`) so an agent never silently guesses.

🕰️ **History-wide discovery (`excavate`)** — Background-ingested commit/diff/stash/reflog index (never blocks a query) that can locate code no longer present at HEAD. Feeds `rescue`'s per-file version plan (likely-breaking commit flagged, recommended last-known-good candidate) and its gated apply — working-tree only, dirty files refused unless a strategy (`--merge` / `--stash-first` / `--allow-dirty`) is given.

🔀 **Deterministic branch sync (`reconcile`)** — One call: fetch, classify (`up_to_date` / `fast_forwarded` / `ahead` / `diverged`), and act. Diverged-but-clean can rebase via an explicit opt-in (proven conflict-free by `git merge-tree` before touching the worktree, backed up first); a real conflict returns a structured report (base/ours/theirs hunks per path) instead of silent failure.

🔒 **Safe git mutations** — `inspect`, `review` (surfaces conflicts, never hides them), `history`, `diff`, `publish`, `push`, `ship`, `branch`, `sync`, `update` — snapshot-token gated, locked, and journaled for crash-safe, idempotent retries.

📈 **Ranking signals** — Recency/churn activity scoring and live session context (recent edits, active errors) rerank candidates *within* tiers only — never promoting a stale-but-touched file across a tier boundary.

⚡ **Serving** — One core `Service`; CLI one-shot commands transparently use a warm Unix-socket daemon (NDJSON protocol, fs watcher, idle timeout) when available.

🎯 **Error sniper** — Per-repository SQLite sink for runtime/HTTP-5xx/build/test failures, queried via `pixel sniper` or its stdio MCP server.

🧠 **Transcript recall** — Machine-wide hybrid lexical+semantic retrieval over every LLM CLI's transcripts.

## 🔧 Installation

### From source (release build)

```bash
git clone https://github.com/LivioGama/pixel.git
cd pixel
cargo build --release
# → target/release/pixel
```

### Activate

```bash
pixel install   # registers the pixel MCP server, removes stale gitpixel/usable-git registrations
                 # (only if `pixel mcp` is runnable — otherwise old servers are preserved),
                 # installs the guard + SessionStart hooks, rewrites CLAUDE.md/AGENTS.md via
                 # managed markers. Supports --dry-run to preview changes first.
pixel doctor     # should report green
```

### Workspace layout

| Crate | Role |
|-------|------|
| `pixel-index` | Trigram/sparse index, shard, plan, verify, freshness overlay |
| `pixel-graph` | Tree-sitter extraction, call resolution, concept index, analyses |
| `pixel-context` | Token-budgeted context assembly |
| `pixel-facts` | History/commit/diff/stash/reflog background ingest — powers `excavate` |
| `pixel-rank` | Pure RRF fusion core + activity/session reranking signals |
| `pixel-git` | Unified git subprocess wrapper (timeout + output-byte-cap enforcement, credential redaction, ref-injection validation) |
| `pixel-ops` | Semantic git operations — snapshot store, repo lock, operation journal, publish/push/branch/update/reconcile |
| `pixel-proto` | Shared contract types — envelope, error codes, snapshot tokens, the `Op` enum |
| `pixel-daemon` | `Service`, daemon, NDJSON API |
| `pixel-session` | Error sink: store, dedup, query layer, run wrapper, MCP server |
| `pixel-recall` | Transcript recall: ingest, trigram + semantic search |
| `pixel-install` | Installer, doctor, migrate, config rewriting |
| `pixel` | `pixel` binary — every command surface |
| `pixel-bench` | Criterion benchmarks + the gitpixel parity harness |

## 🚀 Quick Start

Indexing is lazy — no bootstrap step needed. Just run commands directly:

```bash
# Regex search, ranked
pixel search 'handleClick' /path/to/repo --scope code

# Resolve a phrase to exact code
pixel resolve "the form" /path/to/repo --json

# Task-scoped file targeting (mandatory first step for implementation tasks)
pixel targets "add rate limiting to the upload endpoint" /path/to/repo

# Recover deleted/historical code
pixel excavate --phrase "upload progress" /path/to/repo --json
pixel rescue "upload progress was working before" /path/to/repo --json

# One-call branch sync
pixel reconcile /path/to/repo

# Blast radius before editing
pixel impact someFunction /path/to/repo --direction upstream
```

## 📖 Agent Rules

Mandatory usage rules live at [`~/.agent-config/rules/pixel.md`](https://github.com/LivioGama/pixel-rules) and the full usage skill at `~/.agent-config/skills/pixel/SKILL.md` — both enforce pixel over raw git/`rg`/manual search for the four core scenarios: historical code recovery, phrase/label/error resolution, deterministic branch sync, and task scoping.

## 📄 License

MIT (derived code from hypergrep, MIT; ClickHouse sparse-grams algorithm, Apache-2.0 — see NOTICE).
