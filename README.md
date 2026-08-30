# 📋 pixel

> **Deterministic retrieval + git engine for LLM agents.**

If an answer can be retrieved instantly and with certainty from repo state, pixel retrieves it — no grepping, no manual git archaeology, no LLM reasoning about things a query can just answer. The LLM only steps in for genuine ambiguity, like a real merge conflict.

Inspired by [GitNexus](https://github.com/abhigyanpatwari/GitNexus) (code knowledge graph) and [Stacklit](https://github.com/glincker/stacklit) (compact codebase context for agents). Born from waiting 30 minutes for Claude to `git push`.

[![Status](https://img.shields.io/badge/status-active-success)](https://github.com/LivioGama/pixel)
[![Language](https://img.shields.io/badge/Rust-2024%20edition-orange)](https://www.rust-lang.org/)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)
[![agent-config](https://img.shields.io/badge/agent--config-managed-blue)](https://github.com/LivioGama/pixel-rules)

<a href="https://liviogama.github.io/agent-config/redirect.html?url=https://raw.githubusercontent.com/LivioGama/pixel-rules/main/rules/pixel.md"><img src="https://raw.githubusercontent.com/LivioGama/agent-config/main/assets/install-badge-small.jpg" alt="Install pixel rules" height="40" /></a>

---

## The problem

90% of LLM coding agent work falls into four scenarios. Today, agents handle all of them by reasoning through Git — scrolling diffs, guessing commits, running repetitive searches, and burning tokens on operations that should be deterministic.

### 1. Recovering deleted code

You want a feature back. The code exists in Git history, a stale branch, or a stash.

- Agent wanders through `git log`, reads diffs, guesses the commit
- Does `git checkout` of the old version — erases fixes, destroys uncommitted work, reintroduces regressions
- Sometimes forgets to check the stash entirely
- Should be instant and safe — not a 15-minute archaeology expedition ending in lost work

### 2. Locating code by a phrase or error

You paste a label that exists in exactly one place.

- Agent runs 12 searches to find it
- You say "change the form" — one meaningful form exists, agent goes spelunking
- You report a 503 — finite set of endpoints can produce it, agent doesn't know which are likely
- No awareness of what you're testing or just changed
- Should be milliseconds for unique strings, ranked by context for ambiguous ones

### 3. Synchronizing branches

You ask to sync with main. No conflict — clean fast-forward.

- Agent spends 15s reasoning through `git fetch`, `git status`, `git merge`
- Maybe panics about force-push
- A deterministic script could handle it in under a second
- AI should handle conflicts only — everything else stays deterministic

### 4. Scoping a task before editing

You say "fix the login bug."

- Agent reads 20 files, edits 15, breaks 3 unrelated features, takes 40 min
- Nothing told it which files matter
- Git history + code graph + call tree can produce a closed P0/P1/P2 file list
- Agent should get that list before touching anything
- Guard hook should block edits to files not on it

---

## 💡 What this looks like

Numbers are illustrative (order-of-magnitude), not a benchmark — the source diagrams live in `docs/examples/` and get regenerated once real measurements replace them.

<table>
<tr><td align="center" width="50%"><b>Recovering deleted code</b></td><td align="center" width="50%"><b>Locating code by an error</b></td></tr>
<tr><td align="center" width="50%"><img src="docs/examples/01-recover-deleted-code.svg" width="600" alt="Recovering deleted code" /></td><td align="center" width="50%"><img src="docs/examples/02-resolve-error.svg" width="600" alt="Locating code by an error" /></td></tr>
<tr><td align="center" width="50%"><b>Syncing a branch</b></td><td align="center" width="50%"><b>Starting a task</b></td></tr>
<tr><td align="center" width="50%"><img src="docs/examples/03-reconcile-branch.svg" width="600" alt="Syncing a branch" /></td><td align="center" width="50%"><img src="docs/examples/04-targets-task.svg" width="600" alt="Starting a task" /></td></tr>
<tr><td align="center" width="50%"><b>Searching the codebase</b></td><td align="center" width="50%"><b>Blast radius before editing</b></td></tr>
<tr><td align="center" width="50%"><img src="docs/examples/05-search-ranked.svg" width="600" alt="Searching the codebase" /></td><td align="center" width="50%"><img src="docs/examples/06-impact-blast-radius.svg" width="600" alt="Blast radius before editing" /></td></tr>
<tr><td align="center" width="50%"><b>Finding real callers</b></td><td align="center" width="50%"><b>Checking impact before committing</b></td></tr>
<tr><td align="center" width="50%"><img src="docs/examples/07-uses-callers.svg" width="600" alt="Finding real callers" /></td><td align="center" width="50%"><img src="docs/examples/08-changes-precommit.svg" width="600" alt="Checking impact before committing" /></td></tr>
<tr><td align="center" width="50%"><b>Committing a change</b></td><td align="center" width="50%"><b>Reviewing a conflict</b></td></tr>
<tr><td align="center" width="50%"><img src="docs/examples/09-publish-commit.svg" width="600" alt="Committing a change" /></td><td align="center" width="50%"><img src="docs/examples/10-review-conflicts.svg" width="600" alt="Reviewing a conflict" /></td></tr>
</table>

---

## ✨ Features

| Feature | Description |
|---------|-------------|
| **Search** | Indexed, git-anchored, ranked. `search --scope code` |
| **Code graph** | Tree-sitter across TS/TSX/JS/Rust/Go/Java/Python: `symbol`, `impact`, `uses`, `trace`, `changes`, token-budgeted `context` |
| **Resolve** | A phrase, label, or error → exact code, with an honest confidence level (`resolved` / `ranked` / `unresolved`) |
| **Excavate + rescue** | Finds code no longer at HEAD (deleted, stashed, on another branch) and restores it safely, refusing dirty files without a strategy |
| **Reconcile** | One-call branch sync: fetch, classify, act; real conflicts get a structured report, not silence |
| **Git mutations** | `publish`/`push`/`branch`/`update` etc., snapshot-token gated and crash-safe |
| **Ranking signals** | Recency and live session context rerank results, never promoting a stale file above a better match |
| **Daemon** | A warm background process keeps everything sub-millisecond; falls back to in-process automatically |

---

## 📖 Agent Rules

pixel enforces mandatory workflows through **CLI + hooks**, not MCP:

- **Recovery**: `pixel rescue` / `pixel excavate` — restore deleted/stashed code
- **Resolution**: `pixel resolve` — find code by error/phrase/label
- **Branch sync**: `pixel reconcile` — one-call fetch + classify + act
- **Task scoping**: `pixel targets` — closed file list before editing

Rules: [`~/.agent-config/rules/pixel.md`](https://github.com/LivioGama/pixel-rules) · Skill: `~/.agent-config/skills/pixel/SKILL.md`

The guard hook blocks destructive git commands and unscoped edits in indexed repos. Bypass with `PIXEL_TARGETS_GUARD=0`.

### Agent tool support

| Tool | Guard hooks | How |
|------|------------|-----|
| Claude Code | ✅ | `~/.claude/settings.json` |
| Devin | ✅ | `~/.config/devin/config.json` |
| Codex | ✅ | `~/.codex/hooks.json` |
| Gemini CLI | ✅ | `~/.gemini/settings.json` |
| Cursor | ❌ | No hook support (VSCode extension) |
| OpenCode | ❌ | No hook support |
| Zed | ❌ | No hook support |

---

## 📄 License

MIT (derived code from hypergrep, MIT; ClickHouse sparse-grams algorithm, Apache-2.0 — see `NOTICE`).
