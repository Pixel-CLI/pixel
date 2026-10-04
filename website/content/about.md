---
title: "About"
description: "Pixel is made by two co-founders who were tired of watching coding agents reread the same code. Why they built it, how they work, and how to reach them."
---

<!-- The people come from data/team.toml ({{% team %}}), which the home's JSON-LD reads too: change them there, never here. The contact channels restate SECURITY.md and CONTRIBUTING.md. Every figure here comes from a shortcode or a page that states its method: add none without its source. -->

Two developers make Pixel, in the open, and use it every day to build Pixel itself. It is free, MIT-licensed, and its index stays on your machine. Every figure we publish comes with the command that reproduces it, and we list the benchmarks we lose next to the ones we win.

## The problem we kept paying for

For Livio, it did not make sense to wait 15 seconds to watch an LLM commit and push. Tired of seeing models do archaeology in the Git history to retrieve trivial things, after building [usable-git](https://github.com/LivioGama/usable-git) and experimenting with the slowness of [GitNexus](/vs/gitnexus/), he decided to check what phone Google released after the Nexus — and that is how GitPixel was born. But, very quickly, the "git" prefix did not make sense anymore, and this was the ["just Facebook" moment](https://en.wikipedia.org/wiki/History_of_Facebook#TheFacebook).

For Navid, it started at work, on a large monolith. The coding agents were good at the hard part and wasteful at everything before it. Ask one to change a method and it would open the whole model, then the whole controller, then the whole service it called, just to learn the names of things. The same files came back into the context session after session. We were paying for reading, not for reasoning.

That was not the agent's fault. It had no way to ask "who calls this?" or "what is in this file?" without reading the file. Real codebases are like that, not like the tidy projects in the demos, and that is where the waste costs the most.

## What we built instead

Pixel turns those questions into commands: the signatures of a file instead of its body, the callers of a function, the commit that changed a line. Each answer fits a token budget and says whether it is complete, capped or unresolved. On large files from well-known projects, the agent reads {{% read-savings "summary" %}} ([the files and the method](/benchmarks/#well-known-files)).

An agent believes whatever it is told, so we hold ourselves to two rules:

- **An answer says what it does not know.** A static call graph never sees every caller, so Pixel never claims it does.
- **A number is only true with its source.** The [benchmarks](/benchmarks/) name the version and the command, and they say where another tool does better: GitNexus finds more Ruby callers, and semble has the right file in its top 10 more often.

Read [why Pixel uses Rust](/why-rust/) for the engineering tradeoffs, the limits of our language audit, and where other languages could fit.

## Who we are

Pixel has two co-founders, who share the project as equals:

{{% team %}}

Livio started Pixel and gave it its shape. The first commit, on 29 August 2026, brought in the workspace it grew from. Navid brought the monolith problem above and taught Pixel to read Ruby and Rails. We work on the same `main`, and today every change reaches it through a public pull request.

Everyone else who has contributed code is on the repository's [contributors page](https://github.com/Pixel-CLI/pixel/graphs/contributors).

## Fun fact

Pixel has a `pixel-qa` bot that uses Pixel to fix pull requests efficiently: the bot runs the same retrieval and guarded Git writes you get, and closes the loop on its own PRs.

## Why you can trust it on your code

- **We use it before you do**: Pixel is built with coding agents that run Pixel. Its hooks are active in every session we open on its repository, so a regression hits us first.
- **We write down our mistakes**: the rules in the repository's [`.agents/rules/`](https://github.com/Pixel-CLI/pixel/tree/main/.agents/rules) come from things that went wrong, often with the pull request that paid for them. Our agents read them before editing, and so can you.
- **Open source**: the code, its history and every review are public on [GitHub](https://github.com/Pixel-CLI/pixel), under the [MIT License](https://github.com/Pixel-CLI/pixel/blob/main/LICENSE).
- **Local by design**: the index stays in `.pixel/` on your machine, and the binary sends no telemetry ([Privacy Policy](/legal/privacy-policy/)).
- **Public releases**: each version is a tag on `main`, with its [changelog](https://github.com/Pixel-CLI/pixel/blob/main/CHANGELOG.md) and a [release](https://github.com/Pixel-CLI/pixel/releases) you can follow as a feed.
- **Easy to leave**: one `pixel uninstall` undoes the setup.

## Try it on your own code

The best way to judge Pixel is on your own code. [Install Pixel](/#install), then point it at the largest file in your repository:

```bash
pixel list-signatures path/to/a/large/file
```

The first result compares what a full read would have cost with what Pixel returned, in tokens. If it saves you nothing, tell us why: that is the report we learn the most from.

## Talk to us

A question about how you use Pixel is as welcome as a bug report.

| For | Where |
| --- | --- |
| a question, an idea, or feedback on how you use Pixel | [GitHub Discussions](https://github.com/Pixel-CLI/pixel/discussions) |
| a bug, with the command and its output | [GitHub Issues](https://github.com/Pixel-CLI/pixel/issues) |
| a pull request | [CONTRIBUTING.md](https://github.com/Pixel-CLI/pixel/blob/main/CONTRIBUTING.md) lists what a pull request needs to be merged |
| a security vulnerability, privately | [a private security advisory](https://github.com/Pixel-CLI/pixel/security/advisories/new) ([SECURITY.md](https://github.com/Pixel-CLI/pixel/blob/main/SECURITY.md)) |
| a request about your data | as the [Privacy Policy](/legal/privacy-policy/#who-is-responsible) says |

There is no contact form, so the site keeps nothing you send. Every channel above is on GitHub, where you can read past threads before you post.
