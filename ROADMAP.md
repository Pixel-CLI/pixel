# Roadmap

What Pixel intends to do, and not do, from October 2026 to October 2027.
It states a direction, not a promise: the maintainers (GOVERNANCE.md)
revise it when that direction changes, and at least once a year. Day-to-day
work is tracked on [project 3](https://github.com/users/LivioGama/projects/3);
each theme below links the issues that carry it.

## Direction

Pixel replaces with deterministic answers everything in repository work that
does not need a model: search, symbols, callers, task scope, history, review
and Git, from one local CLI wired into the coding agents people already use.
Its claims are measured, and a measurement that shows no gain is published
too.

## Next twelve months

- **One route from a task to its evidence, in every agent.** The same
  task-aware path from a new prompt to the relevant files, bounded reads and
  the next action in Claude Code, Codex, pi, OpenCode, Copilot CLI, Devin and
  Antigravity, with the metrics line delivered in each
  ([#603](https://github.com/Pixel-CLI/pixel/issues/603)).
- **Evidence before claims.** Reproducible end-to-end case studies against
  capable native tooling, published whether Pixel wins or not
  ([#626](https://github.com/Pixel-CLI/pixel/issues/626),
  [#569](https://github.com/Pixel-CLI/pixel/issues/569)).
- **Retrieval an agent can recover from.** Typed recovery hints that tell an
  empty result from a malformed request, missing coverage or an exhausted
  budget ([#625](https://github.com/Pixel-CLI/pixel/issues/625)); bounded
  recursion-cycle audits on the graph
  ([#622](https://github.com/Pixel-CLI/pixel/issues/622)); version-pinned
  reference corpora for dependencies
  ([#623](https://github.com/Pixel-CLI/pixel/issues/623)).
- **Cheaper classification.** An opt-in verified-history tier before the
  local classifier, kept only if it measures better
  ([#624](https://github.com/Pixel-CLI/pixel/issues/624)).
- **Security and supply chain.** A published threat model and assurance case
  ([#685](https://github.com/Pixel-CLI/pixel/issues/685),
  [#731](https://github.com/Pixel-CLI/pixel/issues/731)), reproducible
  release builds ([#733](https://github.com/Pixel-CLI/pixel/issues/733)),
  measured test coverage
  ([#732](https://github.com/Pixel-CLI/pixel/issues/732)), and the OpenSSF
  Best Practices silver level.
- **Releases.** Frequent small releases from `main`, each with its notes,
  signed provenance and SBOM; only the latest release receives security
  fixes (SECURITY.md).

## Not planned

- **No account, no hosted service, no telemetry.** Pixel runs on the
  user's machine and sends nothing on its own beyond the release check
  SECURITY.md describes.
- **No MCP server.** 0.7.0 removed them; every capability is a CLI command
  that agents call through their shell and Pixel's hooks.
- **No model in the default path.** Deterministic retrieval stays the
  default; model-backed commands (`pixel classify`, `pixel web-search`)
  remain opt-in.
- **No rewrite of the runtime in another language** unless an audit shows it
  keeps Pixel's correctness and speed and pays for the migration
  ([#526](https://github.com/Pixel-CLI/pixel/issues/526) prepares that audit
  only).
