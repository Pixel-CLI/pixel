# Releasing

Always loaded: the only sanctioned release path.

- **To release, run the `release` skill** (`.agents/skills/release/SKILL.md`).
  It owns the whole flow: version pick, `prepare.sh` (changelog cut +
  lockstep version bump + `Cargo.lock` + `check-release`) in a
  `release-x.y.z` pull request into `main`, then the `vX.Y.Z` tag on its
  merge, which triggers `release.yml`.
- **`main` is the only long-lived branch.** Pull requests target it by
  default, and a release is a tag on its history. The one exception is a
  patch that cannot wait for `main` to be releasable: its fix still merges
  into `main`, then a maintenance-release pull request targets a
  `release/x.y` branch cut from the line's last tag (see the skill).
- A request to "ship", "tag", "publish" or "release" a version — or a failed
  Release workflow run — is the `release` skill's trigger. Do not improvise
  the steps; the skill encodes them and its scratchpad record
  (`release-x.y.z.md`) survives a context reset.

- **Keep the validated candidate exact.** Record base and prepare SHAs; run
  the skill's candidate guard before merge and before tag. Base drift means
  renewed changelog coverage and gates, never a `BEHIND` bypass.
- **Security releases start privately.** Read the skill's
  `references/security-release.md` before naming a patched version or
  importing an advisory. Validate the private diff before import; verify the
  release before publishing the advisory and embargoed threat-model details.
  A protection exception needs explicit maintainer authorization and verified
  restoration; a refused client merge is handed off, not retried blindly.

## Missing Rust CodeQL configuration (verified 2026-11)

A PR can remain `BLOCKED` with green required jobs when the aggregate
`CodeQL` check reports a missing `/language:rust` configuration: `codeql.yml`
excludes Rust from `pull_request` runs but includes it on manual dispatch.
Before proposing an admin bypass, run `gh workflow run codeql.yml --ref
<branch>` for the recorded PR head and watch that run in the background.
Confirm its `headSha` still equals the PR head, then recheck the aggregate
CodeQL verdict and mergeability; dispatch success alone is not sufficient.
Verified during 0.11.0: run 42544490355 on #1407's `e3455be6` cleared the
missing-configuration block, and the normal merge succeeded. Actual alerts
or a failed scan still need triage; this does not authorize bypassing them.
