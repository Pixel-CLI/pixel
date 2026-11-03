# Brief recall replay: ref-candidate

- binary: `pixel 0.8.0`, sha256 `02f4fe7b7ce7`
- labels: sha256 `302a272b6ead`, 218 replayed rows, 3 in unindexed repositories (left out)
- passes: 3 (the last is reported); rows whose verdict changed between the last two: 0
- daemons at the end: 9 of the binary under test, 0 of another binary, 0 none

| group | rows | project | follow-up | other | briefed | project recall | follow-up recall | precision | hit@8 (pool) |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| all | 218 | 54 | 40 | 124 | 124 | 89% | 82% | 65% | 53% (38) |
| claude | 139 | 34 | 31 | 74 | 80 | 88% | 87% | 71% | 46% (26) |
| codex | 61 | 14 | 9 | 38 | 32 | 86% | 67% | 56% | 43% (7) |
| pi | 18 | 6 | 0 | 12 | 12 | 100% | n/a | 50% | 100% (5) |
