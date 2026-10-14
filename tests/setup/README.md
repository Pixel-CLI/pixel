# `pixel setup` golden files

Every file under `.agents/` is **exactly** what `pixel setup` writes into one
agent's instruction file, for the default feature selection. One file per agent
per scope, so the whole matrix is readable in a diff instead of inferred from
the code.

```text
.agents/
  config.yaml                 the global settings the default features store
  global/<agent>.md           $HOME and the harness config under it
  repository/<agent>.md       one repository and the harness config inside it
```

Two scopes because they are two layouts: a machine has `~/.claude/CLAUDE.md`, a
repository has `CLAUDE.md`. Four agents have no file in the machine scope
(Cursor, Devin, Antigravity and the shared `AGENTS.md` target): their harness
reads the project's file only, or needs frontmatter pixel does not write. The
review screen reports that rather than guessing a path, and there is no golden
for them.

Each file is the managed block and nothing else — the user's own text around a
block is preserved by the upsert, and that is a contract test next to the code
(`crates/pixel-install/src/setup/files.rs`), not a golden.

## Regenerate

The wording lives in `crates/pixel-install/src/setup/feature.rs`. After an
intentional change:

```bash
PIXEL_SETUP_UPDATE_GOLDENS=1 cargo test -p pixel-install --lib setup::goldens
```

Then **read the diff**. A golden that moves when it should not is the test doing
its job: it is the only place that shows what every agent's file ends up
containing, byte for byte, without running the command.

## How it is verified

`crates/pixel-install/src/setup/goldens.rs` renders and applies the setup for
one agent at a time in a scratch directory and compares the result with the
file here. It never goes through the command line: the `--selected-agents`,
`--selected-features` and `--dummy-apply` flags that drive the wizard from a
script are removed before the release, and this comparison has to outlive them.

The command itself is covered by `crates/pixel/tests/cli/setup_cli.rs`
(`--print`, the no-terminal refusal, and the files a scripted run writes).
