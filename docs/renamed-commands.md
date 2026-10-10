# Renamed commands

Every subcommand got a verb-first name after 0.2.4 (for example `prepare-repo`
instead of `ready`). The old names stay accepted as hidden aliases until
**1.0**, so scripts, hook entries and agent prompts written for 0.2.x keep
working. Invoking an old name prints one line on stderr naming the new one:

```
note: 'ready' is now 'prepare-repo'; the old name stays accepted until 1.0
```

The note is never written to stdout, so `--json` output is unchanged. Like
the metrics line, it is silenced by `--metrics off` or `PIXEL_METRICS=0`,
and never appears in hook responses or `search-like-rg` output. Protocol op
names and JSON fields did not change. `migrate` was removed: it now exits 0
with a note and does nothing.

| Old name | New name |
| --- | --- |
| `ask` | `search-meaning` |
| `branch` | `new-branch` |
| `branches` | `list-branches` |
| `changes` | `what-changed` |
| `clusters` | `list-areas` |
| `context` | `pack-context` |
| `excavate` | `dig-history` |
| `graph` | `rebuild-graph` |
| `history` | `commit-history` |
| `history-search` | `search-history` |
| `hook` | `run-hook` |
| `index` | `build-index` |
| `inspect` | `repo-state` |
| `lifecycle` | `file-history` |
| `log` | `action-log` |
| `map` | `repo-map` |
| `processes` | `list-flows` |
| `publish` | `commit` |
| `query` | `run-recipe` |
| `ready` | `prepare-repo` |
| `reconcile` | `sync-branch` |
| `release-check` | `check-release` |
| `replay-flow` | `flow` |
| `rescue` | `plan-rollback` |
| `resolve` | `find-code` |
| `review` | `review-changes` |
| `rewrite` | `squash-branch` |
| `savings` | `token-savings` |
| `search` | `search-content` |
| `search-compat` | `search-like-rg` |
| `ship` | `commit-and-push` |
| `skeleton` | `list-signatures` |
| `sniper` | `list-errors` |
| `stats` | `index-stats` |
| `symbol` | `find-symbol` |
| `sync` | `fetch` |
| `targets` | `scope-task` |
| `task` | `task-state` |
| `trace` | `call-path` |
| `update` | `fast-forward` |
| `upgrade` | `self-update` |
| `uses` | `who-calls` |
