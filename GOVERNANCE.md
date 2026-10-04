# Governance

Pixel is maintained by two people who share every role. This file lists who
can reach the project's sensitive resources and what each role covers, so a
contributor or a security reporter knows who decides and who holds what.

## Maintainers

| Maintainer | GitHub | Access |
| --- | --- | --- |
| Livio Gamassia | [@LivioGama](https://github.com/LivioGama) | Owner of the Pixel-CLI organization; admin of `Pixel-CLI/pixel`; owner of `LivioGama/homebrew-tap` |
| Navid EMAD | [@navidemad](https://github.com/navidemad) | Member of the Pixel-CLI organization; admin of `Pixel-CLI/pixel` |

Both maintainers hold every role below; either one can act alone, and
neither needs the other's approval to do so.

## Roles and responsibilities

- **Review and merge.** Read and review pull requests, answer CodeRabbit's
  findings (CONTRIBUTING.md, "CodeRabbit reviews"), and merge into `main`.
  Only the maintainers push branches to this repository and merge into
  `main`.
- **Release.** Cut releases with the `release` skill: the prepare pull
  request into `main`, then the `vX.Y.Z` tag that runs the release workflow
  (CONTRIBUTING.md, "Release (maintainers)").
- **Security.** Receive and triage private vulnerability reports
  (SECURITY.md), fix them, and publish the advisory and the release.
- **Dependencies and CI.** Review Dependabot updates, `cargo-deny`, CodeQL and
  OpenSSF Scorecard findings, and keep the workflows' permissions minimal.
- **Project board.** Keep [project 3](https://github.com/users/LivioGama/projects/3)
  in step with the pull requests (`.agents/rules/project-task.md`).

## Sensitive resources and who holds them

| Resource | Who |
| --- | --- |
| Pixel-CLI organization settings | @LivioGama (owner) |
| Repository settings, rulesets, branch protection, Actions settings | @LivioGama, @navidemad |
| Repository secrets: `HOMEBREW_TAP_TOKEN` (pushes the Homebrew formula), `PROJECTS_TOKEN` (syncs the project board), `VT_API_KEY` (VirusTotal submissions) | @LivioGama, @navidemad |
| Release tags (`v*`, immutable by ruleset) and the release workflow's signing identity | @LivioGama, @navidemad |
| `LivioGama/homebrew-tap` | @LivioGama |
| Private vulnerability reports | @LivioGama, @navidemad |

## Secrets and credentials

The project's credentials are the three repository secrets above. The rules
for them:

- **Storage.** Only as GitHub encrypted repository secrets: never in the
  repository, an issue, a pull request, a log or a chat. Secret scanning with
  push protection is enabled on the repository to stop one being committed.
- **Access.** Only the maintainers can create, read or replace a secret. Each
  one is read by the workflow that needs it and passed through an environment
  variable, never echoed: `HOMEBREW_TAP_TOKEN` by `release.yml` (publish and
  smoke jobs), `PROJECTS_TOKEN` by `board-sync.yml`, `VT_API_KEY` by
  `release.yml` (the VirusTotal job, which hands it to `curl` through a 0600
  header file). A fork's pull request reaches no secret but the board sync's,
  which never checks out the fork's code.
- **Scope.** Each credential grants the least it needs: `HOMEBREW_TAP_TOKEN`
  is a fine-grained token with Contents read/write on `LivioGama/homebrew-tap`
  only; `PROJECTS_TOKEN` writes the project board; `VT_API_KEY` belongs to an
  account used only for release scans.
- **Rotation.** Tokens are created with an expiry (at most one year for
  GitHub tokens) and replaced before it. Every credential is revoked at its
  provider and replaced at once when a maintainer's access changes or when it
  may have been exposed; the provider's usage log is then checked for misuse.

## Granting access

Write or admin access to this repository, a seat in the Pixel-CLI
organization, or access to a secret is granted only after both maintainers
have reviewed the person's contributions (their merged pull requests and how
they handle review) and agreed, in a pull request that updates this file.
Access no longer needed is removed the same way, and the maintainers review
the list above whenever one of them changes role.

## Contributors

Anyone can contribute, from a fork: open a pull request from your fork's
branch into `main`; CONTRIBUTING.md is the guide. Only the two maintainers
have write access to this repository, so only they push branches here and
open pull requests from them; a regular contributor may be given the Triage
role to label and manage issues and pull requests, never write access. A
fork's pull request runs CI only after a maintainer approves it. Its
workflows receive no secrets, except the metadata-only `pull_request_target`
board sync, which receives `PROJECTS_TOKEN` to update the project board and
never checks out the fork's code.

## Changes to this file

A change to the maintainers or their access goes through a pull request that
updates this file in the same change as the access itself.
