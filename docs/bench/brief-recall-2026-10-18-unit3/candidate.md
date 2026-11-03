# Brief recall replay: candidate

- binary: `pixel 0.8.0`, sha256 `4fbae5699bf3`
- labels: sha256 `302a272b6ead`, 218 replayed rows, 3 in unindexed repositories (left out)
- passes: 3 (the last is reported); rows whose verdict changed between the last two: 3
- daemons at the end: 9 of the binary under test, 0 of another binary, 0 none

| group | rows | project | follow-up | other | briefed | project recall | follow-up recall | precision | hit@8 (pool) |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| all | 218 | 54 | 40 | 124 | 133 | 89% | 82% | 61% | 58% (38) |
| claude | 139 | 34 | 31 | 74 | 88 | 91% | 84% | 65% | 48% (27) |
| codex | 61 | 14 | 9 | 38 | 31 | 79% | 78% | 58% | 67% (6) |
| pi | 18 | 6 | 0 | 12 | 14 | 100% | n/a | 43% | 100% (5) |
