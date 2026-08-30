# Real measurements — 2026-08-30T01:06:02Z

Machine: dia, 4 cores. Fixture: /tmp/pixel-example-fixture (synthetic, small).
Methodology: wall-clock via bash , best of 3 warm runs (daemon started first). Tokens estimated as response_bytes/4 (rough proxy, not a real tokenizer count).

| Scenario | Time | Response size | Est. tokens |
|---|---|---|---|
| 01 rescue plan (recover deleted) | 7ms | 76B | ~19 tok |
| 02 resolve (locate by error) | 6ms | 337B | ~84 tok |
| 03 reconcile (branch sync, up-to-date case) | 64ms | 273B | ~68 tok |
| 04 targets (task scoping) | 8ms | 1110B | ~277 tok |
| 05 search --scope code | 7ms | 407B | ~101 tok |
| 06 impact (blast radius) | 6ms | 699B | ~174 tok |
| 07 uses (callers) | 6ms | 981B | ~245 tok |
| 08 changes (pre-commit) | 48ms | 330B | ~82 tok |
| 09 inspect (pre-publish state) | 41ms | 1367B | ~341 tok |
| 10 review (conflict surfacing) | 16ms | 494B | ~123 tok |

Note: 03/04/09/10 substitute a safe read-only op for the write path shown in the diagram (reconcile's up-to-date case, targets instead of a live rescue --apply, inspect/review instead of publish) so this script never mutates a real repo or requires --request-id bookkeeping.
