# Specs

The contracts Pixel keeps: what a caller, an agent host or a user may rely on,
stated so that a change which breaks one fails a test or a review. They are
written from the code on this branch; a claim here that the code does not
bear out is a defect in the spec.

ARCHITECTURE.md answers "how is it built": which crate owns what, the
on-disk layout, the install writers, the per-host extension points. A spec
answers "what is promised": inputs, outputs, guarantees and what happens when
something fails. A spec links to ARCHITECTURE.md for the structure and never
copies it, and ARCHITECTURE.md links here for the promise. Where the two
disagree, the code decides and both are fixed in the same pull request.

## The specs

| File | Contract |
|---|---|
| [`hooks.md`](hooks.md) | `pixel run-hook`: providers and events, what `pixel setup` registers (every command with its fail-open tail), exit 0 and one JSON object or nothing, the decision window and the answer deadline, blocking by JSON only, a brief split over several hooks |
| [`brief.md`](brief.md) | `pixel brief` and the `[PIXEL:BRIEF]` block: size caps, who gets one, the only two decisions that may silence it (a relevance score never does), the optional classifier admission, time, the region pack and its no-line-twice rule, the decision record |
| [`classify.md`](classify.md) | `pixel classify`: the opt-in `classify.enabled` switch, engines and presets, key resolution and masking, the `--jsonl` stream, what a remote engine receives |
| [`metrics.md`](metrics.md) | the three local records, the workflow estimates and what they do not claim, the live lines and relays, `pixel metrics` |
| [`config.md`](config.md) | the config layers and `pixel config <key> <value>`: validation, which layer wins, never through a link |
| [`search-history.md`](search-history.md) | `pixel search-history`: facets, ranking, `--read-only`, the index state a result carries |
| [`ops.md`](ops.md) | the daemon socket: NDJSON request, envelope, error codes, `PROTOCOL_VERSION`, the one absolute `deadline_unix_ms` |
| [`state-and-threats.md`](state-and-threats.md) | how files are written, what is kept and for how long, what leaves the machine, repository content as data |

## Rules for a spec

- **Open with `Status:`** and say what the file is normative for. A spec states
  a guarantee, not a history: no "was", no retired mechanism (the changelog and
  `git log` keep that).
- **Name the code, not a memory of it.** Constants, env vars and paths as the
  code spells them, checked against the tree before the push.
- **Say what was observed and what was not.** A behaviour run on the binary says
  so; a claim read from the code only is not dressed as a run.
- **End with "Tests that prove it".** A guarantee no test holds is written as
  such, not implied.
- **A change that moves a promise edits its spec in the same pull request**,
  and lists `specs/` under "Docs touched" next to `ARCHITECTURE.md` when both
  move (`.agents/rules/architecture-doc.md`).

## The drift test

`cargo test -p pixel-cli --test cli specs_drift::` holds the parts of the specs
a machine can read:

- every subcommand of `pixel --help` has one row in the command index below, in
  `--help` order, and a row that names a spec file names one that exists and
  mentions the command as `` `pixel <name>` ``;
- the provider and event lists of `hooks.md` are the ones `pixel run-hook`
  accepts;
- the settings of `config.md` are the ones `pixel config <key> <value>` accepts;
- the facets of `search-history.md` are the ones `--help` lists;
- the protocol version in `ops.md` is `pixel_daemon::api::PROTOCOL_VERSION`;
- every backticked `` `pixel <name>` `` in a spec is a real command, the rule
  `docs_drift` applies to the other docs.

## Command index

A command with `—` has no spec yet; its description stays in ARCHITECTURE.md,
"Command surface". The index is the list a new command has to join.

| Command | Spec | Note |
|---|---|---|
| `build-index` | — |  |
| `search-content` | — |  |
| `search-like-rg` | — |  |
| `run-recipe` | — |  |
| `search-meaning` | — |  |
| `search-history` | `search-history.md` |  |
| `scope-task` | — |  |
| `brief` | `brief.md` | the block the prompt-submit hook injects too |
| `execution-brief` | — |  |
| `find-symbol` | — |  |
| `list-signatures` | — |  |
| `repo-map` | — |  |
| `pack-context` | — |  |
| `impact` | — |  |
| `who-calls` | — |  |
| `rename` | — |  |
| `call-path` | — |  |
| `evaluate` | — |  |
| `cycles` | — |  |
| `list-flows` | — |  |
| `list-areas` | — |  |
| `what-changed` | — |  |
| `rebuild-graph` | — |  |
| `graph-shadow` | — |  |
| `workspace` | — |  |
| `index-pack` | — |  |
| `index-unpack` | — |  |
| `reference` | — |  |
| `status` | — |  |
| `coverage` | — |  |
| `audit` | — |  |
| `metrics` | `metrics.md` | the live lines and the local records as well |
| `prepare-repo` | — |  |
| `daemon` | `ops.md` | the socket, the request, the envelope and the deadline; process control is in ARCHITECTURE.md |
| `classify` | `classify.md` | `classify-eval` and `classify-history` are pointers only |
| `classify-eval` | — |  |
| `classify-history` | — |  |
| `review-gate` | — |  |
| `find-code` | — |  |
| `setup` | `hooks.md` | only what it registers for each host; the install writers are in ARCHITECTURE.md |
| `install` | — |  |
| `uninstall` | — |  |
| `check-release` | — |  |
| `self-update` | — |  |
| `doctor` | — |  |
| `run-hook` | `hooks.md` |  |
| `config` | `config.md` |  |
| `task-state` | — |  |
| `plan` | — |  |
| `prompts` | — |  |
| `plugin` | — |  |
| `completions` | — |  |
| `man` | — |  |
| `help` | — |  |
