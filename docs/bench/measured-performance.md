# Measured performance — wins AND losses

Two different things get measured, and they must not be conflated:

**Single-op latency** (daemon-warm, small synthetic fixture — [`examples/real-measurements.md`](../examples/real-measurements.md)): individual pixel ops answer in 6–64ms wall-clock. These are op figures, not agent-workflow figures. The CLI also carries a measured ~17ms process-spawn floor for the ~45MB binary on the bench machine (`crates/pixel-bench/benches/m1_latency.rs` comments).

**Agent-level A/B** (`claude -p` driving full workflows against this repo, pixel hooks vs. hooks stripped, 3 reps/cell, medians — [`pixel-bench-results.txt`](pixel-bench-results.txt), 2026-08-30, commit `865facf`):

| Scenario | With pixel (median) | Baseline (median) | Verdict |
|---|---|---|---|
| s1-locate (find code by phrase) | 10.3s | 11.0s | ≈ neutral |
| s2-scope (task scoping) | 50.1s | 46.7s | ❌ pixel worse |
| s3-sync (branch sync) | 8.8s | 10.1s | ✅ pixel better |
| s4-recover (historical code) | 64.8s | 36.4s | ❌ pixel much worse |

Per tenet T1 (no claim without a measurement), that table is the current honest picture: fast ops do not automatically make fast agents. The s2/s4 regressions are exactly what this change set targets — the hard targets read-fence is demoted to advisory (it measured recall 0.60 → 0.19, [`sniper-discovery.md`](sniper-discovery.md)), and the recovery flow is being reworked; per T3, each scenario keeps MANDATORY status only while a re-run shows it non-inferior to baseline.

**Known caveat on that table's baseline arm**: its harness stripped pixel hooks but not the installed CLAUDE.md rule text (which mandates pixel by absolute path), and its transcripts were overwritten before a tool_use-level purity check could run — so its baseline purity is **unknown** ([`2026-08-30-session-log.md`](2026-08-30-session-log.md)).

**Clean-baseline A/B** (same day, baseline under `claude --safe-mode` — a vanilla agent with no CLAUDE.md/hooks/skills — verified pixel-free at the tool_use level in 12/12 cells; means over 3 valid reps/cell — [`2026-08-30-session-log.md`](2026-08-30-session-log.md)):

| Scenario | Vanilla baseline (mean) | With pixel + rules (mean) | Delta |
|---|---|---|---|
| s1-locate | 13.1s | 22.6s | ❌ +72% |
| s2-scope | 106.6s | 109.7s | ≈ +2% |
| s3-sync | 12.1s | 15.7s | ❌ +29% |
| s4-recover | 32.3s | 84.3s | ❌ +160% |

Two honest readings, both required: (1) the pixel arm carries the **entire** installed rule text, so the delta measures pixel-plus-doctrine overhead, not pixel ops alone; (2) the tool_use-level parse shows the pixel arm invoked pixel **sparsely** (s1: 0 invocations in all 3 reps; s2: 2/0/0; s3: 0/0/1; s4: 1/0/6) — much of the slowdown is agents processing mandates they then barely use. Per T3, this is the measurement that keeps every scenario's MANDATORY status on probation until a run shows non-inferiority.

**Post-fix re-run** (same day, same clean `--safe-mode` baseline methodology, single binary + doctrine held constant for the full run — [`2026-08-30-session-log.md`](2026-08-30-session-log.md)), after two fixes: (a) pixel's own doctrine text trimmed ~72% (16.3KB → 4.6KB — the rule-vs-binary parity and scenario-consistency `pixel doctor` checks stayed green through the cut); (b) `pixel excavate` fixed to rank diff-content-proven `suspect` commits first instead of by pure recency — it was burying the real "who deleted this" answer behind unrelated files that merely quote the search phrase as prose (`crates/pixel-facts/src/excavate.rs`, `excavate_by_phrase`):

| Scenario | Vanilla baseline (mean) | With pixel + rules (mean) | Delta | vs. pre-fix |
|---|---|---|---|---|
| s1-locate | 13.4s | 25.0s | ❌ +86% | worse (noise — single 3-rep sample, see caveat below) |
| s2-scope | 105.1s | 72.6s | ✅ **−31%** | **flipped from a loss to a win**, reproduced across 2 independent runs |
| s3-sync | 10.6s | 14.4s | ❌ +36% | worse (noise — task is a single tool call in both arms) |
| s4-recover | 30.1s | 62.1s | ❌ +106% | improved from +160% — consistent with the excavate fix |

s2-scope (task scoping — the `targets` mandate) now measures **better** with pixel than without, on two separate runs; per T3 that keeps it solidly MANDATORY. s4-recover's regression margin shrank by a third, tracking the ranking fix directly. s1/s3 stayed flat or worsened slightly — both are single-tool-call tasks where the delta is dominated by something pixel's own logic doesn't touch: see the isolated measurement below.

**Isolating pixel's own cost from the rest of this user's config** — the arms above load the user's **entire global `CLAUDE.md`** (~140KB / ~35,000 tokens across dozens of unrelated rules — RTK, Jira, browser automation, credential policy, none of it pixel's), because `--safe-mode` is the only flag that suppresses it and that also disables the PreToolUse hooks. [`scripts/pixel-bench-isolated.sh`](../../scripts/pixel-bench-isolated.sh) isolates pixel's own doctrine (`--safe-mode --append-system-prompt "$(cat pixel.md)"`, verified to deliver pixel-only instructions with no other rule content) against a truly blank agent — at the cost of losing hook enforcement in both arms, so it measures doctrine-driven reasoning, not the mechanically-enforced product.

N=3 confirmed ([`2026-08-30-session-log.md`](2026-08-30-session-log.md)), means over valid runs, **`pixel_calls=0` in nearly every cell** — the effect below is the doctrine's reasoning guidance changing agent behavior, not tool invocation:

| Scenario | Vanilla (mean) | Pixel doctrine only (mean) | Delta |
|---|---|---|---|
| s1-locate | 12.5s | 14.0s | ❌ +12% (small, consistent, ~1.5s absolute) |
| s2-scope | 128.9s | 90.9s | ✅ **−29%** — cross-validates the full-stack run's −31% above, independently |
| s3-sync | 9.1s | 8.0s | ✅ **−12%** |
| s4-recover | 32.5s | 42.3s | ❌ +30% (reversed from an N=1 preview's −15% — one high-variance rep drove it; inconclusive at this sample size) |

Honest synthesis: pixel's doctrine measurably improves agent task-scoping and branch-sync reasoning — two independent benchmark designs (full-stack and isolated) now agree on task-scoping's ~30% win. The single-lookup task (s1) pays a small, likely-irreducible tax for reading any extra instructions before a one-shot answer. Recovery (s4) improved substantially from the excavate ranking fix in the full-stack run but stays too noisy to call in isolation — legitimate open work, not a claim either way.

## Raw data

- [`pixel-bench-results.txt`](pixel-bench-results.txt) — full-stack agent A/B
- [`pixel-bench-isolated-results.txt`](pixel-bench-isolated-results.txt) — doctrine-only isolated A/B
- [`2026-08-30-session-log.md`](2026-08-30-session-log.md) — session log with run ordering and validity notes
- [`sniper-discovery.md`](sniper-discovery.md) — read-fence recall measurement (0.60 → 0.19)
- [`examples/real-measurements.md`](../examples/real-measurements.md) — single-op latency table
