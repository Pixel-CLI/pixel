# On-disk state and threats

Status: normative. A change that adds a file, a hook, an install target, a
network destination or a secret edits this file in the same pull request.

## State

Per repository, under `.pixel/` (git-ignored):

| Path | Owner | Contents |
|---|---|---|
| `base.shard`, `delta.shard`, `state.json`, `build.lock` | `pixel-index` | trigram index layers |
| `graph.db`, `index.lock`, `checked` | `pixel-graph` | one SQLite graph, generation per transaction |
| `history.db` (+`-wal`, `-shm`, `.lock`) | `pixel-index` | history facts, demand-driven |
| `code-vectors/` | `pixel-index` (`semantic`) | chunk vectors |
| `actions.jsonl` | `pixel-metrics` | one line per invocation, rotated at 5 MiB |
| `journal.jsonl` | `pixel-metrics` | typed hook records, last 1,500 lines, 0600 |
| `brief-sessions/<sid-hash>.json` | `pixel-brief` | the ranges of region packs this session was already sent; 0600, files older than 24 h swept |
| `sessions/<sid>.json` | `pixel-daemon` | carded symbols, pending stale set (`hooks.md`); 0600 |
| `brief-parts/<sid>-<prompt>.txt` | `pixel` hook | the whole brief of a split delivery; deleted by part 1, entries older than 1 h swept |
| `plugins/<name>/` | `pixel-plugin` | repo plugins (trusted only at the recorded digest) |
| `config.yaml` | `pixel config` | repository settings over the global ones |

Machine-wide: `~/.pixel/config.yaml` (remote keys, 0600), `~/.pixel/setup-receipt.json`,
`~/.pixel/plugins/`, `trusted.toml`, `plugins.toml`, `~/.local/share/pixel/pi-package/`
and `pi-classify/`, `~/.local/share/pixel/models/`, the base-shard cache
`~/.cache/pixel/shards`, the daemon socket (`ops.md`). Skill and hook files live in
each agent's own directories (`setup-migration.md`).

**Nothing in `.pixel/` is trusted by name.** A repository can commit `.pixel/`,
links included. Every store calls `sidecar::check` on open (a link or a
git-tracked `.pixel` is refused with the command to delete it); every file is
written through `nofollow`; every directory through `private_dir`; SQLite opens
with `SQLITE_OPEN_NOFOLLOW`; paths read back from a shard or the graph are used
only after `repo_path` confines them to the root.

## Actors and boundaries

| Actor | Trust |
|---|---|
| the user | trusted |
| the agent and its model | semi-trusted; steerable by prompt injection from anything it reads, including Pixel's own output |
| repository content | untrusted when the repo is not the user's own (files, commit messages, a committed `.pixel/`, git config) |
| other local users | untrusted; file and socket permissions only |
| network services | untrusted for integrity (classify endpoints, model downloads, the release check) |

B1 hostile repo → parse/index/git → B3 agent context. B2 agent → CLI and hooks.
B4 same-user process → daemon socket (read-only ops only). B5 `pixel setup` → agent
configs (hooks run on every tool call). B6 network. B7 contributor → CI →
release.

## Threats and mitigations (summary; the full model is written at M9)

1. **Prompt injection through Pixel's output.** Everything Pixel injects is
   repository data. Briefs, cards and blocks carry an untrusted-data trailer
   where the host allows it, quote rather than paraphrase, and never contain
   instructions from repository text; the footer is Pixel's own constant.
2. **A crafted hook payload.** Bounded read (1 MiB, 1 s), strict parsing, paths
   confined to the repository, shell commands never executed, only inspected for
   named repository paths.
3. **Hostile source files.** Tree-sitter extraction is bounded by size and
   depth caps; a file past them is indexed unread; a panic in a hook is caught.
4. **A repository ships `.pixel/` or git config that runs code.** Sidecar
   trust boundary above; git invoked only through `GitRunner` with hardened
   config and argument guards.
5. **`pixel setup` damages agent configs.** Receipt, backups, atomic writes, key-wise
   edits, byte-exact uninstall (`setup-migration.md`).
6. **Secrets.** Credential flags masked in every sink; keys stored 0600 in
   `~/.pixel/config.yaml` or read from Infisical per invocation; credential-shaped
   paths never named in the journal; `review` reports a possible secret by
   pattern class and anchor, never the value.
7. **Daemon abuse.** Owner-only socket; no write ops; request size and time
   limits; a version skew is refused, not guessed.
8. **Supply chain.** Pinned action SHAs, `cargo deny`, SLSA provenance and SBOM
   at release (M9), reproducible-build job.
