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
  Merging into `main` is reserved to the maintainers.
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

## Contributors

Anyone can contribute through a pull request; CONTRIBUTING.md is the guide.
Contributors who work on the repository directly may be given write access
to push branches: [@xDelph](https://github.com/xDelph) has it today. Write
access does not include the secrets, the settings or releases, and merging
into `main` stays with the maintainers.

## Changes to this file

A change to the maintainers or their access goes through a pull request that
updates this file in the same change as the access itself.
