# Brief recall replay: heldout-candidate

- binary: `pixel 0.8.0`, sha256 `02f4fe7b7ce7`
- labels: sha256 `05a472f49243`, 191 replayed rows, 14 in unindexed repositories (left out)
- passes: 3 (the last is reported); rows whose verdict changed between the last two: 0
- daemons at the end: 10 of the binary under test, 0 of another binary, 0 none

| group | rows | project | follow-up | other | briefed | project recall | follow-up recall | precision | hit@8 (pool) |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| all | 191 | 29 | 66 | 96 | 109 | 69% | 67% | 59% | 64% (14) |
| claude | 77 | 10 | 23 | 44 | 44 | 100% | 61% | 55% | 60% (10) |
| codex | 113 | 19 | 43 | 51 | 65 | 53% | 70% | 62% | 75% (4) |
| pi | 1 | 0 | 0 | 1 | 0 | n/a | n/a | n/a | n/a (0) |
