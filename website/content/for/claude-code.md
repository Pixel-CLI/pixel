---
title: "Pixel for Claude Code"
description: "What pixel install writes into Claude Code's settings, the per-repository cleanup, the plugin alternative, how to check the wiring and how to remove it."
agent: "claude-code"
---

<!-- Setup sections: data/agents.toml through layouts/shortcodes/agent-setup.html. The figure below restates content/benchmarks.md#on-whole-agent-tasks: change it there first. -->

{{% agent-setup %}}

## Evidence and limits

An Opus 5.5 medium trial compared 11 runs per arm on one low-research scoping task. Median duration was 42.9 s without Pixel and 47.6 s with install hooks (+11%); median tokens read rose 12% and provider-reported API cost rose 31%. This is one task, not a general result for your sessions. Raw runs are not archived; [known setup and provenance limits](../../benchmarks/#on-whole-agent-tasks) explain what can and cannot be checked.
