# 📋 pixel

> **Deterministic retrieval + git engine for LLM agents.**

If an answer can be retrieved deterministically — and with its certainty stated — from repo state, pixel retrieves it: no grepping, no manual git archaeology, no LLM reasoning about things a query can just answer. Every op returns a complete answer, an explicitly-bounded partial answer, or a structured ambiguity report; the LLM only steps in for genuine ambiguity, like a real merge conflict.

Inspired by [GitNexus](https://github.com/abhigyanpatwari/GitNexus) (code knowledge graph) and [Stacklit](https://github.com/glincker/stacklit) (compact codebase context for agents). Born from waiting 30 minutes for Claude to `git push`.

[![Status](https://img.shields.io/badge/status-active-success)](https://github.com/LivioGama/pixel)
[![Language](https://img.shields.io/badge/Rust-2024%20edition-orange)](https://www.rust-lang.org/)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)
[![agent-config](https://img.shields.io/badge/agent--config-managed-blue)](https://github.com/LivioGama/pixel-rules)

<a href="https://liviogama.github.io/agent-config/redirect.html?url=https://raw.githubusercontent.com/LivioGama/pixel-rules/main/rules/pixel.md"><img src="https://raw.githubusercontent.com/LivioGama/agent-config/main/assets/install-badge-small.jpg" alt="Install pixel rules" height="40" /></a>

---

## The problem

Most of the annoying LLM coding agent work falls into five scenarios. Today, agents handle them by reasoning through Git — scrolling diffs, guessing commits, running repetitive searches, and burning tokens on operations that should be deterministic.

### 1. Locating code by a phrase or error

You paste a label that exists in exactly one place.

- Agent runs 12 searches to find it
- You say "change the form" — one meaningful form exists, agent goes spelunking
- You report a 503 — finite set of endpoints can produce it, agent doesn't know which are likely
- No awareness of what you're testing or just changed
- Should be milliseconds for unique strings, ranked by context for ambiguous ones

### 2. Scoping a task before editing

You say "fix the login bug."

- Agent reads 20 files, edits 15, breaks 3 unrelated features, takes 40 min
- Nothing told it which files matter
- Git history + code graph + call tree can produce a prioritized P0/P1/P2 file list
- Agent should get that list before touching anything
- Guard hook warns on edits outside it (a hard block was tried and measured harmful — recall 0.60 → 0.19, [`docs/bench/sniper-discovery.md`](docs/bench/sniper-discovery.md))

### 3. Synchronizing branches

You ask to sync with main. No conflict — clean fast-forward.

- Agent spends 15s reasoning through `git fetch`, `git status`, `git merge`
- Maybe panics about force-push
- A deterministic script could handle it in under a second
- AI should handle conflicts only — everything else stays deterministic

### 4. Recovering deleted code

You want a feature back. The code exists in Git history, a stale branch, or a stash.

- Agent wanders through `git log`, reads diffs, guesses the commit
- Does `git checkout` of the old version — erases fixes, destroys uncommitted work, reintroduces regressions
- Sometimes forgets to check the stash entirely
- Should be instant and safe — not a 15-minute archaeology expedition ending in lost work

### 5. Editing blind

You ask for a one-line change to a widely-called function.

- Agent edits it without knowing who calls it
- Breaks callers it never read; the breakage surfaces two tasks later
- The call graph could answer "who depends on this?" deterministically before the edit
- Should be a milliseconds-scale check (`impact`/`changes`), not a post-hoc debugging session

---

## 📊 Measured performance — wins AND losses

Two different things get measured, and they must not be conflated:

**Single-op latency** (daemon-warm, small synthetic fixture — [`docs/examples/real-measurements.md`](docs/examples/real-measurements.md)): individual pixel ops answer in 6–64ms wall-clock. These are op figures, not agent-workflow figures. The CLI also carries a measured ~17ms process-spawn floor for the ~45MB binary on the bench machine (`crates/pixel-bench/benches/m1_latency.rs` comments).

**Agent-level A/B** (`claude -p` driving full workflows against this repo, pixel hooks vs. hooks stripped, 3 reps/cell, medians — [`pixel-bench-results.txt`](pixel-bench-results.txt), 2026-08-30, commit `865facf`):

| Scenario | With pixel (median) | Baseline (median) | Verdict |
|---|---|---|---|
| s1-locate (find code by phrase) | 10.3s | 11.0s | ≈ neutral |
| s2-scope (task scoping) | 50.1s | 46.7s | ❌ pixel worse |
| s3-sync (branch sync) | 8.8s | 10.1s | ✅ pixel better |
| s4-recover (historical code) | 64.8s | 36.4s | ❌ pixel much worse |

Per tenet T1 (no claim without a measurement), that table is the current honest picture: fast ops do not automatically make fast agents. The s2/s4 regressions are exactly what this change set targets — the hard targets read-fence is demoted to advisory (it measured recall 0.60 → 0.19, [`docs/bench/sniper-discovery.md`](docs/bench/sniper-discovery.md)), and the recovery flow is being reworked; per T3, each scenario keeps MANDATORY status only while a re-run shows it non-inferior to baseline.

**Known caveat on that table's baseline arm**: its harness stripped pixel hooks but not the installed CLAUDE.md rule text (which mandates pixel by absolute path), and its transcripts were overwritten before a tool_use-level purity check could run — so its baseline purity is **unknown** ([`docs/bench/2026-08-30-session-log.md`](docs/bench/2026-08-30-session-log.md)).

**Clean-baseline A/B** (same day, baseline under `claude --safe-mode` — a vanilla agent with no CLAUDE.md/hooks/skills — verified pixel-free at the tool_use level in 12/12 cells; means over 3 valid reps/cell — [`docs/bench/2026-08-30-session-log.md`](docs/bench/2026-08-30-session-log.md)):

| Scenario | Vanilla baseline (mean) | With pixel + rules (mean) | Delta |
|---|---|---|---|
| s1-locate | 13.1s | 22.6s | ❌ +72% |
| s2-scope | 106.6s | 109.7s | ≈ +2% |
| s3-sync | 12.1s | 15.7s | ❌ +29% |
| s4-recover | 32.3s | 84.3s | ❌ +160% |

Two honest readings, both required: (1) the pixel arm carries the **entire** installed rule text, so the delta measures pixel-plus-doctrine overhead, not pixel ops alone; (2) the tool_use-level parse shows the pixel arm invoked pixel **sparsely** (s1: 0 invocations in all 3 reps; s2: 2/0/0; s3: 0/0/1; s4: 1/0/6) — much of the slowdown is agents processing mandates they then barely use. Per T3, this is the measurement that keeps every scenario's MANDATORY status on probation until a run shows non-inferiority.

**Post-fix re-run** (same day, same clean `--safe-mode` baseline methodology, single binary + doctrine held constant for the full run — [`docs/bench/2026-08-30-session-log.md`](docs/bench/2026-08-30-session-log.md)), after two fixes: (a) pixel's own doctrine text trimmed ~72% (16.3KB → 4.6KB — the rule-vs-binary parity and scenario-consistency `pixel doctor` checks stayed green through the cut); (b) `pixel excavate` fixed to rank diff-content-proven `suspect` commits first instead of by pure recency — it was burying the real "who deleted this" answer behind unrelated files that merely quote the search phrase as prose (`crates/pixel-facts/src/excavate.rs`, `excavate_by_phrase`):

| Scenario | Vanilla baseline (mean) | With pixel + rules (mean) | Delta | vs. pre-fix |
|---|---|---|---|---|
| s1-locate | 13.4s | 25.0s | ❌ +86% | worse (noise — single 3-rep sample, see caveat below) |
| s2-scope | 105.1s | 72.6s | ✅ **−31%** | **flipped from a loss to a win**, reproduced across 2 independent runs |
| s3-sync | 10.6s | 14.4s | ❌ +36% | worse (noise — task is a single tool call in both arms) |
| s4-recover | 30.1s | 62.1s | ❌ +106% | improved from +160% — consistent with the excavate fix |

s2-scope (task scoping — the `targets` mandate) now measures **better** with pixel than without, on two separate runs; per T3 that keeps it solidly MANDATORY. s4-recover's regression margin shrank by a third, tracking the ranking fix directly. s1/s3 stayed flat or worsened slightly — both are single-tool-call tasks where the delta is dominated by something pixel's own logic doesn't touch: see the isolated measurement below.

**Isolating pixel's own cost from the rest of this user's config** — the arms above load the user's **entire global `CLAUDE.md`** (~140KB / ~35,000 tokens across dozens of unrelated rules — RTK, Jira, browser automation, credential policy, none of it pixel's), because `--safe-mode` is the only flag that suppresses it and that also disables the PreToolUse hooks. [`scripts/pixel-bench-isolated.sh`](scripts/pixel-bench-isolated.sh) isolates pixel's own doctrine (`--safe-mode --append-system-prompt "$(cat pixel.md)"`, verified to deliver pixel-only instructions with no other rule content) against a truly blank agent — at the cost of losing hook enforcement in both arms, so it measures doctrine-driven reasoning, not the mechanically-enforced product.

N=3 confirmed ([`docs/bench/2026-08-30-session-log.md`](docs/bench/2026-08-30-session-log.md)), means over valid runs, **`pixel_calls=0` in nearly every cell** — the effect below is the doctrine's reasoning guidance changing agent behavior, not tool invocation:

| Scenario | Vanilla (mean) | Pixel doctrine only (mean) | Delta |
|---|---|---|---|
| s1-locate | 12.5s | 14.0s | ❌ +12% (small, consistent, ~1.5s absolute) |
| s2-scope | 128.9s | 90.9s | ✅ **−29%** — cross-validates the full-stack run's −31% above, independently |
| s3-sync | 9.1s | 8.0s | ✅ **−12%** |
| s4-recover | 32.5s | 42.3s | ❌ +30% (reversed from an N=1 preview's −15% — one high-variance rep drove it; inconclusive at this sample size) |

Honest synthesis: pixel's doctrine measurably improves agent task-scoping and branch-sync reasoning — two independent benchmark designs (full-stack and isolated) now agree on task-scoping's ~30% win. The single-lookup task (s1) pays a small, likely-irreducible tax for reading any extra instructions before a one-shot answer. Recovery (s4) improved substantially from the excavate ranking fix in the full-stack run but stays too noisy to call in isolation — legitimate open work, not a claim either way.

## 💡 What this looks like

The diagrams below illustrate the intended flows. The "without pixel" columns are **illustrative, not measured**; the pixel op time/token figures are single-op measurements from [`docs/examples/real-measurements.md`](docs/examples/real-measurements.md). For real end-to-end agent numbers, see the A/B table above.

<table>
<tr><td align="center" width="50%"><b>Locating code by an error</b></td><td align="center" width="50%"><b>Starting a task</b></td></tr>
<tr><td align="center" width="50%"><img src="docs/examples/02-resolve-error.svg" width="600" alt="Locating code by an error" /></td><td align="center" width="50%"><img src="docs/examples/04-targets-task.svg" width="600" alt="Starting a task" /></td></tr>
<tr><td align="center" width="50%"><b>Syncing a branch</b></td><td align="center" width="50%"><b>Recovering deleted code</b></td></tr>
<tr><td align="center" width="50%"><img src="docs/examples/03-reconcile-branch.svg" width="600" alt="Syncing a branch" /></td><td align="center" width="50%"><img src="docs/examples/01-recover-deleted-code.svg" width="600" alt="Recovering deleted code" /></td></tr>
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
| **Daemon** | A warm background process keeps *service-time* sub-millisecond (CLI end-to-end still pays a ~17ms spawn floor); falls back to in-process automatically |

---

## 📖 Agent Rules

pixel enforces five scenarios through **CLI + hooks**, not MCP:

- **Recovery** (mandatory): `pixel rescue` / `pixel excavate` — restore deleted/stashed code
- **Resolution** (mandatory): `pixel resolve` — find code by error/phrase/label
- **Branch sync** (mandatory): `pixel reconcile` — one-call fetch + classify + act
- **Task scoping** (advisory): `pixel targets` — mandatory *first call* of a task, but the returned list is a starting set, not a read-fence (the hard fence measured recall 0.60 → 0.19, [`docs/bench/sniper-discovery.md`](docs/bench/sniper-discovery.md))
- **Blast radius** (mandatory): `pixel impact` / `pixel changes` — callers and affected flows before any edit

Rules: [`~/.agent-config/rules/pixel.md`](https://github.com/LivioGama/pixel-rules) · Skill: `~/.agent-config/skills/pixel/SKILL.md`

The guard hook blocks destructive git commands and warns on edits outside the active targets list in indexed repos. Bypass with `PIXEL_TARGETS_GUARD=0`.

### Agent tool support

| Tool | Guard hooks | How |
|------|------------|-----|
| Claude Code | ✅ | `~/.claude/settings.json` |
| Devin | ✅ | `~/.config/devin/config.json` |
| Codex | ✅ | `~/.codex/hooks.json` |
| Gemini CLI | ✅ | `~/.gemini/settings.json` |
| zcode | ✅ | `~/.zcode/cli/config.json` (PreToolUse + SessionStart) |
| pi | ⚠️ rules only | `~/.pi/agent` memory file — extension API, no PreToolUse hooks |
| Cursor | ❌ | No hook support (VSCode extension) |
| OpenCode | ❌ | No hook support |
| Zed | ❌ | No hook support |

---

## 📄 License

MIT (derived code from hypergrep, MIT; ClickHouse sparse-grams algorithm, Apache-2.0 — see `NOTICE`).
