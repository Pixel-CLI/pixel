# `pixel setup` golden files

Two sets of goldens pin what `pixel setup` writes, so the result is readable in
a diff instead of inferred from the code.

```text
.agents/
  config.yaml   the global settings the default features store
  skill.md      the exact skills/pixel/SKILL.md every harness's skills dir gets
goldens/
  <agent>.txt   every file one agent's setup writes (path, mode, bytes), the
                backups beside the files it changes, and what --remove leaves
  shared/       files every agent's run writes alike (~/.pixel/config.yaml)
```

`pixel setup` writes no instruction file, rules file or managed block: guidance
reaches the agents as the `pixel` skill only, so `skill.md` is the one text
every harness reads.

## Regenerate

The feature wording lives in `crates/pixel-setup/src/setup/feature.rs`, the
skill text in `crates/pixel-setup/src/skill.rs` and
`crates/pixel-setup/assets/pixel-agent-prompt.md`. After an intentional change:

```bash
PIXEL_SETUP_UPDATE_GOLDENS=1 cargo test -p pixel-setup --lib setup::goldens
```

and, for `goldens/`:

```bash
PIXEL_SETUP_UPDATE_GOLDENS=1 cargo nextest run -p pixel-cli -E 'test(/setup_goldens_cli::/)'
```

Then **read the diff**. A golden that moves when it should not is the test doing
its job.

## How it is verified

`crates/pixel-setup/src/setup/goldens.rs` renders the plan and the skill in
process and compares them with the files in `.agents/`. It never goes through the
command line: the `--selected-agents`, `--selected-features` and
`--dummy-apply` flags that drive the wizard from a script are removed before the
release, and this comparison has to outlive them.

The command itself is covered by `crates/pixel/tests/cli/setup_cli.rs`
(`--print`, the no-terminal refusal, and the files a scripted run writes).

`goldens/` goes through the command line on purpose: it is the byte-level
record of each agent's install and uninstall, one test per agent of
`--selected-agents`, in `crates/pixel/tests/cli/setup_goldens_cli.rs` (its
module doc lists what is normalised). It is marked `TEMPORARY (dev-flags)`
because it uses those flags; when they go, it is ported to the wizard's typed
answers, not deleted.

Global setup also writes a private ownership receipt. The per-agent tests require
removal to return the seeded HOME byte for byte, apart from reported backups.
Receipt fingerprints are rendered as `{SHA256:<deployed path>}` from the actual
file bytes, whose content the same golden pins. Separate tests cover edited
files, preexisting skills/directories/YAML, and a removal preview with no writes.
