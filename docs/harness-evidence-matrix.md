# Six-Harness Evidence Matrix

Template for recording live verification of Pixel's retrieval route across all
six agent harnesses. Fill one column per harness after a real session challenge.

## How to use

1. Restart all six harnesses in a Herdr-managed pane (Claude, Codex, Pi,
   agy/Antigravity, OpenCode, Devin).
2. Send the same read-only task to each pane. A backticked-identifier task
   exercises the exact-search route; a behavior task exercises find-code.
3. For each agent, capture:
   - the first Pixel command it ran (verbatim)
   - whether the empty-result fallback ran exactly once
   - the read range it used after Pixel served a path (must be ≤40 lines)
   - whether the `🟩 pixel <subcommand> ❀ <ms>` metrics line was visible
   - whether the hook returned without blocking the session
4. Record evidence class: `direct` (command visible in transcript),
   `self-report` (agent described the route from context), `inference`
   (deduced from side effects), `unknown` (not determinable).
5. Note any gap (e.g. read exceeded 40 lines, no fresh Pixel command, hook
   blocked the session).

## Template

Task: `<one-sentence task>`  
Date: `<YYYY-MM-DD>`  
Commit: `<git rev-parse HEAD>`  
Binary: `pixel --version`  
Index: `pixel doctor . --only facts.freshness`

| Harness | Pane | First Pixel Command | Empty Fallback | Read Range | Metrics Line | Evidence Class | Gap |
|---------|------|---------------------|----------------|------------|--------------|----------------|-----|
| **Claude** | `w4:pX` | `pixel search-content -F '<id>' --fallback-query '<task>' --no-daemon` | `N/A (exact hit)` or `ran once` | `≤40 lines` or `>40` | `visible` or `not visible` | `direct` | `<gap or none>` |
| **Codex** | `w4:pX` | `rtk pixel search-content -F '<id>'` | `N/A (exact hit)` or `ran once` | `≤40 lines` or `>40` | `visible` or `not visible` | `direct` | `<gap or none>` |
| **Pi** | `w4:pX` | `<command>` or `not visible` | `N/A` or `ran once` | `≤40` or `>40` or `unbounded` | `visible` or `not visible` | `direct` or `self-report` | `<gap or none>` |
| **agy** | `w4:pX` | `<command>` or `not visible` | `N/A` or `ran once` | `≤40` or `>40` or `unbounded` | `visible` or `not visible` | `direct` or `self-report` | `<gap or none>` |
| **OpenCode** | `w4:pX` | `<command>` or `not visible` | `N/A` or `ran once` | `≤40` or `>40` or `unbounded` | `visible` or `not visible` | `direct` or `self-report` | `<gap or none>` |
| **Devin** | `w4:pX` | `<command>` or `not visible` | `N/A` or `ran once` | `≤40` or `>40` or `unbounded` | `visible` or `not visible` | `direct` or `self-report` | `<gap or none>` |

## Pass criteria

| Check | Pass when |
|-------|-----------|
| First command | Agent's first tool call is `pixel search-content -F '<id>'` (identifier) or `pixel find-code '<concept>'` (behavior) — not `ls`, `grep`, `read`, or `cat` |
| Empty fallback | `search-content -F` returning empty runs exactly one `find-code` fallback in the same command (automatic) or agent runs `find-code` once (route-guided) |
| Bounded read | After Pixel serves `path:line`, agent reads `sed -n '<line>,+40p'` or `read(path, offset=<line>, limit≈40)` — never the whole file |
| Metrics line | `🟩 pixel <subcommand> ❀ <ms> ❀ #<hash>` visible exactly once per Pixel invocation |
| Non-blocking | Hook returns without error; session continues normally |
| Fail-open | Pixel unavailable → agent continues with native retrieval; no error |

## Example (2026-10-04 run)

| Harness | Pane | First Pixel Command | Empty Fallback | Read Range | Metrics Line | Evidence Class | Gap |
|---------|------|---------------------|----------------|------------|--------------|----------------|-----|
| **Claude** | w4:p13 | `pixel search-content -F 'retrieval_route' --fallback-query 'retrieval_route' --no-daemon 2>&1 \| head -60` | N/A (exact hit) | 150 lines (150-300) — exceeds 40-line guidance | Not visible (truncated by `head -60`) | Direct | Read exceeds 40-line route guidance |
| **Codex** | w4:p12 | `rtk pixel search-content -F 'retrieval_route'` | N/A (exact hit) | ≤40 lines (40, 35, 36, 40) ✓ | `🟩 pixel search-content ❀ 168.3ms ❀ #8514a8` visible ✓ | Direct | None — route followed correctly |
| **Pi** | w4:p11 | Not visible (existing context) | N/A | 90 lines (150-239) — exceeds 40-line guidance | Not visible | Self-report | Read exceeds 40-line route guidance; no fresh Pixel command |
| **agy** | w4:p14 | Not visible (existing context) | N/A | Full file read (unbounded) — exceeds 40-line guidance | Not visible | Self-report | Read exceeds 40-line route guidance; no fresh Pixel command |
| **OpenCode** | w4:p15 | Not visible (existing context) | N/A | Not visible | Not visible | Self-report | No fresh Pixel command; answered from context |
| **Devin** | w4:p16 | Not visible (existing context) | N/A | Not visible | Not visible | Self-report | No fresh Pixel command; answered from context |
