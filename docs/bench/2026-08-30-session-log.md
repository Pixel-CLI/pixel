# 2026-08-30 agent-workflow benchmark session log

Raw per-run dumps from this session's investigation live at
`/tmp/pixel-session-archive/` (moved out of the indexed tree — they were
degrading `pixel targets`' own ranking for code-change tasks by scoring as
content matches on plain-English words like "agent"/"tool"/"guard" in prose,
crowding out real source files). This file is the permanent, consolidated
record; the individual `agent-ab-*.txt` dumps are local evidence only.

## Runs, in order

1. **rerun-contaminated** — baseline arm not truly pixel-free (settings-json
   hook-stripping alone leaves the installed CLAUDE.md mandate active).
   INVALID, kept as evidence only.
2. **safemode-baseline** — first clean run (`claude --safe-mode` baseline,
   verified pixel-free at the tool_use level in 12/12 cells). Pre-fix numbers:
   s1 +72%, s2 +2%, s3 +29%, s4 +160% (all pixel worse except essentially tied
   s2).
3. **run3-mixed-binary** — INVALID: binary rebuilt mid-run (excavate ranking
   fix landed partway through). Kept as evidence, not cited.
4. **clean-postfix** (bench4) — single binary held constant; doctrine trimmed
   ~72% (16.3KB→4.6KB) + excavate suspect-first ranking fix. s1 +86%, s2
   **−31%** (win), s3 +36%, s4 +106% (down from +160%).
5. **isolated-n1** / **isolated-n3-confirmed** — doctrine-only comparison
   (`--safe-mode --append-system-prompt`, no hooks, no other rules). N=3: s1
   +12%, s2 **−29%** (win, cross-validates run 4), s3 **−12%** (win), s4 +30%
   (inconclusive, high variance).
6. **bench5-contended** — INVALID: baseline itself inflated 2-4x vs every
   other run (concurrent sibling-session load). Kept as evidence.
7. **bench6-resolve-context-fix** — added: `pixel resolve` now returns inline
   code context (eliminates the mandatory follow-up Read `search` already
   avoided); `pixel excavate` tie-break added (`is_definition` field —
   prefers a real function definition over a same-commit doc-comment mention
   of the same identifier). Binary-hash mismatch flagged mid-run (traced to a
   sibling session's rebuild, not a functional regression — both fixes
   verified present in the installed binary regardless). s1 +18% (best of
   session), s2 +20%, s3 +58%, s4 +117%.
8. **bench7** — added a third fix: doctrine text tells the agent excavate's
   snippet is the answer, don't re-verify with `git show`. s1 +20%, s2 +49%,
   s3 +66%, s4 +91%.

## Aggregate across the 3 clean-ish full-stack runs (4, 6, 7)

| Scenario | Average delta | Pre-fix baseline (run 2) |
|---|---|---|
| s1-locate | +41% | +72% |
| s2-scope | +13% | +2% |
| s3-sync | +53% | +29% |
| s4-recover | +105% | +160% |

## Structural finding

Every tool call in the full-stack "pixel arm" also pays for ~5 unrelated
hooks from other tools on this account (measured: a Node.js `gitnexus` hook
~40ms/call, `rtk hook claude` ~10ms/call, plus `vibe-island-bridge`,
`agent-browser-guard`, `cbm-code-discovery-gate`) and reprocesses the
account's full ~35,000-token global `CLAUDE.md` (of which pixel's own
doctrine is ~1,200 tokens post-trim). Pixel's own hook measured at 0-10ms;
its ops at single-digit milliseconds warm. `--bare` mode (which would allow a
hooks-matched, memory-isolated comparison) categorically rejects every
OAuth-derived credential this account can produce, confirmed three separate
ways including a freshly-minted `claude setup-token` credential — no
combination of flags or credentials this account has access to closes that
gap without either violating the explicit `ANTHROPIC_API_KEY` ban or a
genuine console.anthropic.com API key.
