# `pixel setup` golden files

Two files under `.agents/` pin what `pixel setup` writes for the default
feature selection, so the result is readable in a diff instead of inferred from
the code.

```text
.agents/
  config.yaml   the global settings the default features store
  skill.md      the exact skills/pixel/SKILL.md every harness's skills dir gets
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

Then **read the diff**. A golden that moves when it should not is the test doing
its job.

## How it is verified

`crates/pixel-setup/src/setup/goldens.rs` renders the plan and the skill in
process and compares them with the files here. It never goes through the
command line: the `--selected-agents`, `--selected-features` and
`--dummy-apply` flags that drive the wizard from a script are removed before the
release, and this comparison has to outlive them.

The command itself is covered by `crates/pixel/tests/cli/setup_cli.rs`
(`--print`, the no-terminal refusal, and the files a scripted run writes).
