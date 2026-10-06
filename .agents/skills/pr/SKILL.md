---
name: pr
description: "Write or revise a pixel pull request body: fill .github/pull_request_template.md (Closes #N, Summary with a call-tree, file-tree or pseudocode sketch built from pixel's own graph, before/after Evidence with commands, Merge Danger as door and blast radius, Docs touched). Use when opening a PR, rewriting its description, or answering a reviewer who asks what a PR does or risks. Adapted from Matt Pocock's MIT pr skill and HumanLayer's MIT show-me skill (UPSTREAM pins both)."
---

# Writing the pull request body

The layout is [`.github/pull_request_template.md`](../../../.github/pull_request_template.md);
this skill does not carry a second copy. Fill its sections in order, delete
its HTML comments, skip any preamble and keep prose brief. Use the
vocabulary of `ARCHITECTURE.md` (crate names, op names, `.pixel/` file
names as the code spells them).

## First line

`Closes #<n>` for the issue the PR resolves, `Refs #<n>` when it does only
part of it (`.agents/rules/project-task.md`). After opening, check that the
issue lists the PR under Development: a keyword inside a code span or with
a typo does not link, and `board-sync.yml` reads only what GitHub linked.

## Summary

One or two sentences: what changes and why — the bug reproduced or the
user-visible effect. Then, only when it makes the point faster than the
sentence, the smallest sketch. Use one, sometimes two; never all of them.
The examples below show the shapes; their names are illustrative.

- **Call tree**, for a change in runtime flow. Build it from the graph, not
  from memory: `pixel impact <symbol>` (callers, `--direction downstream`
  for callees) or `pixel who-calls <fn> --role callers`, trimmed to the
  calls the change crosses. Show it as a `diff` when the tree already
  exists:

  ```diff
   ResolveIndex::decide_reference
  -  decide               # no site line: Ruby lexical scope is lost
  +  decide_at            # site_line reaches decide_from
       decide_from
  ```

- **File tree**, for a move, a split or a new module:

  ```diff
   crates/pixel-install/src/
  -├── warp.rs
  +├── warp/
  +│   ├── config.rs     # reads and writes the Warp settings
  +│   └── rules.rs      # the managed rule block
  ```

- **Pseudocode**, for a rule or an algorithm, as a `diff` when it replaces
  one:

  ```diff
   on(pull_request_target)
  -  number = regex("Task (\d+)", body)
  +  issues = pr.closingIssuesReferences
  +  for issue in issues on project 3: set Status
  ```

- **Mermaid** `sequenceDiagram`, for an exchange between processes (CLI,
  daemon, hook, agent) that a tree cannot order.

- The **whole block** instead of a diff only when most of it is new or the
  reader needs a shape to copy (a config entry, a new op's envelope).

Keep only the calls, files and states the change touches. Put each sketch
right after the sentence it supports.

## Evidence

Before and after, each with the command that produced it
(`.agents/rules/measuring.md`: the command beside the number, the
baseline, the run's identity).

- **Best:** a test that fails without the change and passes with it. Name
  it and quote the failing assertion:
  `cargo test -p pixel-graph resolves_ruby_constant_receiver` — before:
  `left: 0, right: 1`; after: ok.
- **Next:** CLI output before and after (`pixel <op> … --json`, trimmed to
  the fields that changed), or a counted result with its command
  ("107 tested, 79 caught, 0 missed", never "green").
- A visual change (the website, `docs/motion`) shows a screenshot or
  capture.

Then one line on what ran where: which CI jobs validate the head, which
local checks you chose, and what was **not** run (the musl cross-build, the
Docker smoke, an installed-hook check). A local pass describes its own
snapshot only (`.agents/rules/validation-loop.md`).

## Merge Danger

**Door.** Two-way when `git revert` restores every user's state; one-way
when it does not. In this repository the usual one-way doors are:

- a bump of `EXTRACTOR_VERSION` (every existing graph rebuilds) or of the
  concept extractor's version;
- `PROTOCOL_VERSION` or the daemon envelope (an older CLI and a newer
  daemon stop understanding each other);
- a format or a path under `.pixel/`, `~/.pixel/` or
  `~/.local/{share,state}/pixel/` (a revert leaves files the old code
  cannot read, or orphans them);
- what `pixel install` writes into Claude, Codex or pi configuration, and
  what `uninstall` removes (users' own settings are in those files);
- a release tag, a published changelog entry, a security advisory;
- anything that deletes or rewrites a user's file.

Say which applies and what a revert would leave behind; otherwise write
"two-way".

**Blast radius.** One word (`none`, `docs`, `CI`, `one op`, `graph`,
`daemon`, `install`, `every agent`), then what a bad merge would break and
for whom: a hook that errors runs in every repository of every user who
installs the release; a ranking change moves every `find-code` answer; a
workflow change can block every pull request.

## Docs touched

`changelog.d/` (or why the change ships nothing), `ARCHITECTURE.md` with
the section `.agents/rules/architecture-doc.md` maps the change to (or why
it moves nothing the map describes), the agent prompt, the README.
