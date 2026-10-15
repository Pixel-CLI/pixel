---
title: "Evaluating Pixel with your team"
description: "What developers, leads, harness engineers and CTOs can check before adopting a local code index."
---

## Developers

Start with `pixel audit` on your repository, then `pixel list-signatures <file>` on a file your agent often opens. The outline helps locate code; read the relevant bodies before editing. [File/outline measurements](/benchmarks/#well-known-files) describe the pinned sample and its parsing limits.

`pixel scope-task` suggests where to start, and `pixel who-calls` finds callers. [Retrieval measurements](/benchmarks/#against-gitnexus) cover the measured latency and caller coverage. Install once with `pixel setup`; [your agent’s page](/for/) explains how it receives the prompt brief and the `pixel` skill.

## Lead developers

`pixel impact` lists callers and callees before a change; `pixel review-changes` shows a structured diff. A static graph can miss dynamic calls, so an empty result is not proof that a change is safe.

[Git operations](/docs/#git-changes), a command plugin, include leased pushes and `pixel plan-rollback`, which writes nothing until `--apply`. `pixel setup --repo` only creates the repository's `.pixel/config.yaml` and adds no hook or guard. [Per repository](/docs/#per-repository) says what it does.

## Harness engineers

Index answers carry completion markers and graph answers state their blind spots. [Reading the answers](/docs/#reading-the-answers) explains how to consume them. The optional [classifier](/classify/) uses a model and is separate from deterministic retrieval.

Pixel uses a CLI plus a skill and hooks. [Context overhead](/benchmarks/#against-gitnexus) and [agent trials](/benchmarks/#on-whole-agent-tasks) describe the tested harnesses: tool use is not proof of lower task time or billed cost. Use `pixel action-log` (a [command plugin](/docs/#command-plugins)) to verify adoption in your own sessions.

## CTOs

The index stays in `.pixel/`, with no account and no telemetry from the binary beyond an optional release check. [The security model](https://github.com/Pixel-CLI/pixel/blob/main/.github/SECURITY.md) names the downloads and optional network operations. Pixel is MIT licensed; [compatibility](/for/) lists the agents that can share the index.

The [savings estimate](/savings/) applies file/outline volume rates to your own assumptions and input price. It does not predict a session’s invoice. Read the [method and losses](/benchmarks/) and [alternatives](/vs/) before deciding.
