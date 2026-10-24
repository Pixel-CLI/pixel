# A Task Exists Before Work Starts

Always loaded: every unit of work in this repository is tracked by an issue,
and the pull request that does the work links that issue the way GitHub itself
understands.

- **The task comes first.** Before starting any work — a fix, a feature, a
  rule, a docs pass — check this repository's issues for one covering it. When
  none exists, open one and link the pull request to it. Pixel has an existing
  [project board](https://github.com/users/LivioGama/projects/3/views/1); this
  candidate does not include its previous board-sync workflow. A conversation
  note is not tracking, and a board status is not proof that a change merged.
- **The PR body opens with `Closes #<number>`.** That closing keyword (or
  `Fixes` / `Resolves`) links the pull request to the issue: GitHub lists it
  under the issue's Development section and closes the issue when the PR
  merges. A PR that does only part
  of an issue writes `Refs #<number>` instead, so the issue stays open; one
  that closes several lists each (`Closes #1, closes #2`). The branch,
  commit subjects and the `.github/changelog.d/` fragment may name the issue too;
  the pull request is one artefact of the task, not the task itself.
- **Keep issue links honest while working.** The pull request's closing
  keyword links it to the issue; check the issue's Development section after
  opening. When a task turns out bigger than its issue says, stop and split
  the issue before continuing.
- **No issue is the exception.** A one-line typo fix, a CI rerun or an
  emergency revert may go without an issue; its PR body then has no closing
  keyword and says in a line why no issue was needed. Everything else links
  its issue as above.

## An issue an agent can pick up

When creating or updating an issue, make the following clear. Scale the
detail to the work: a small fix can cover these points in one paragraph;
no fixed set of headings is required.

- **Problem and outcome.** What fails or is missing, and what should be
  true afterwards. Separate observed evidence from suspected causes.
- **Remaining scope.** Name the unit of work and useful exclusions. An
  epic links its executable children and states what remains at the parent
  level; split independently reviewable work before assigning it as one task.
- **Dependencies and decisions.** Link prerequisites by issue number and
  say what they must deliver before this work can proceed. State which open
  decisions the agent may investigate and resolve, and which need a
  maintainer's decision.
- **Acceptance and evidence.** Give observable completion criteria and
  the proof expected (a regression test, CLI output, a report or a capture).
  For a measured improvement, name the metric, pinned baseline, comparison
  command and meaningful success threshold, with regression guards where
  relevant (`measuring.md`). If the threshold is unknown, make establishing
  it an explicit investigation outcome before promising an implementation.
- **Starting points.** Link the useful reproduction, report, command,
  file or symbol. Include the source revision when the evidence depends on
  it; a complete implementation plan is not required.

When picking an issue, recheck its claims and linked dependencies against
the current code and PR state. Investigate missing technical details;
clarify a missing product decision or conflicting acceptance criteria
before making a dependent change.

When work lands or an issue is split, rewrite the current remaining scope,
dependencies and acceptance at the top. Keep superseded plans and numbers
under a clearly marked historical section, so they cannot be mistaken for
current instructions. Link child issues directly rather than by an informal
label such as "child 0".
