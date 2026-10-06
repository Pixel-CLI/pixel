# A Task Exists Before Work Starts

Always loaded: every unit of work in this repository is tracked by an issue
on the project board, and the pull request that does the work links that
issue the way GitHub itself understands.

- **The task comes first.** Before starting any work — a fix, a feature, a
  rule, a docs pass — check [project 3, view 1](https://github.com/users/LivioGama/projects/3/views/1)
  and the repository's issues for one covering it. When none exists, open
  one (`gh issue create --title "<what and why>"`) and, if you can, add it
  to the project (`gh project item-add 3 --owner LivioGama --url
  <issue-url>`); a contributor outside the organization cannot, and a
  maintainer adds it. A task-out-of-band conversation note is not tracking.
- **The PR body opens with `Closes #<number>`.** That closing keyword (or
  `Fixes` / `Resolves`) links the pull request to the issue: GitHub lists it
  under the issue's Development section and closes the issue when the PR
  merges, and `board-sync.yml` reads the same link. A PR that does only part
  of an issue writes `Refs #<number>` instead, so the issue stays open; one
  that closes several lists each (`Closes #1, closes #2`). The branch,
  commit subjects and the `changelog.d/` fragment may name the issue too;
  the pull request is one artefact of the task, not the task itself.
- **The board follows the PR lifecycle, automatically.** The `Status` field
  on project 3 of every issue the PR closes is set from the pull request
  state by `.github/workflows/board-sync.yml`, never by hand:
  - **In Progress** while the PR is open (at open, reopen, and an edit that
    adds the closing keyword).
  - **Done** when the PR merges — and only then. A green CI alone is not
    Done; a merged PR is.
  - **Todo** when the PR is closed unmerged, so the board never shows
    finished work that does not exist.
  The job reads GitHub's `closingIssuesReferences`, not the body text, so a
  keyword GitHub did not link (inside a code span, a typo, an issue number
  with no issue) moves nothing: check the issue's Development section after
  opening the PR.
- **Keep the board honest while working.** No status may outrun the PR:
  an agent never moves an item to Done as it pushes, and a status set by
  hand ahead of the PR is a claim nothing supports. When a task turns out
  bigger than its issue says, stop and split the issue before continuing.
- **No issue is the exception.** A one-line typo fix, a CI rerun or an
  emergency revert may go without an issue; its PR body then has no closing
  keyword and says in a line why no issue was needed. Everything else links
  its issue as above.
