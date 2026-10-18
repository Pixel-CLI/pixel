# Brief recall replay: baseline

- binary: `pixel 0.8.0`, sha256 `c41d123bfbd7`
- labels: sha256 `302a272b6ead`, 218 replayed rows, 3 in unindexed repositories (left out)
- passes: 3 (the last is reported); rows whose verdict changed between the last two: 3
- daemons at the end: 9 of the binary under test, 0 of another binary, 0 none

| group | rows | project | follow-up | other | briefed | project recall | follow-up recall | precision | hit@8 (pool) |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| all | 218 | 54 | 40 | 124 | 152 | 89% | 80% | 53% | 55% (38) |
| claude | 139 | 34 | 31 | 74 | 98 | 91% | 81% | 57% | 48% (27) |
| codex | 61 | 14 | 9 | 38 | 39 | 79% | 78% | 46% | 50% (6) |
| pi | 18 | 6 | 0 | 12 | 15 | 100% | n/a | 40% | 100% (5) |
