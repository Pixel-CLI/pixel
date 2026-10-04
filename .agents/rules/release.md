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
