<h1 align="center">🟩 Pixel</h1>

<p align="center">
  <strong>Repository evidence, delivered with your prompt.</strong>
</p>

<p align="center">
  <a href="https://github.com/Pixel-CLI/pixel/releases/latest"><img src="https://img.shields.io/github/v/release/Pixel-CLI/pixel?color=2ea043&label=release" alt="Latest release" /></a>
  <a href="https://github.com/Pixel-CLI/pixel/actions/workflows/ci.yml"><img src="https://img.shields.io/github/actions/workflow/status/Pixel-CLI/pixel/ci.yml?branch=main&label=CI" alt="CI" /></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-MIT-blue" alt="MIT license" /></a>
</p>

Pixel gives your coding agent a short, source-cited brief before it starts work. Ask your question normally; Pixel retrieves relevant evidence from a local index and adds it to the agent’s context.

## Save your tokens for the hard part

<p align="center">
  <img src="docs/examples/pixel-prompt-submit-comparison.webp" width="800" alt="Recorded side-by-side Codex run: the same repository question reports 36 seconds with Pixel and 1 minute 58 seconds without, after additional searches and file reads." />
</p>

A uniformly sped-up recording of one Codex run on Requests. CLI-reported time: 36 seconds with Pixel, 1 minute 58 seconds without. Both panes keep their complete recorded output. This is one example, not a token benchmark. [Animation source and limits](docs/motion/prompt-submit/README.md).

Code discovery costs more than the search itself: the model plans a search, calls a tool, reads the result and repeats. Pixel moves repository lookup into the prompt-submit path to cut that discovery loop.

When you press **Enter**, your agent’s `UserPromptSubmit` hook—or the equivalent event in its CLI—runs Pixel against the local index. Pixel builds a compact `[PIXEL:BRIEF]` with relevant definitions, callers, code, tests or history, then adds it alongside your submitted prompt, before the agent’s first response.

The agent starts with cited source evidence already in context. The aim is fewer discovery turns and less repeated context across the whole task. Your agent keeps its usual tools to check the evidence or explore further.

Current main supports prompt briefs in **Claude Code, Codex, pi, Devin, OpenCode, Gemini CLI and Antigravity**, through each host’s hooks or extensions.

## Install

```bash
curl -fsSL https://github.com/Pixel-CLI/pixel/releases/latest/download/install.sh | sh
pixel setup
```

Choose your agents in setup. Then, from each repository you want to work in:

```bash
pixel prepare-repo .
```

Restart your agent session and approve hooks if prompted. Ask your next code question as usual.

The installer downloads the latest release; integrations described here follow `main`. Check the [release notes](https://github.com/Pixel-CLI/pixel/releases/latest), or [build from main](CONTRIBUTING.md#build) for unreleased changes. [Read the installer](scripts/install.sh).

## Preview a brief

```bash
pixel brief "Where is request validation implemented?" .
```

Briefs need an indexed repository. Unrelated prompts or unavailable evidence can produce no brief; your agent can continue with normal search. Set `PIXEL_BRIEF=0` to disable briefs.

Use `pixel doctor .` to check installation and repository health.

Local indexing and lookup need no Pixel account or API key. Optional model-backed features are configured separately.

## `pixel classify`: your local Jev

Use `pixel classify` to choose between named options. The example below is the
same one shown on the [homepage](https://pixel-cli.dev/): it compares three
model tiers for a Rust PR review and returns one probability per label, then
`predicted:` for the highest score.

```bash
pixel classify "Review this Rust PR, find correctness bugs, and propose a safe patch." \
  --engine ollaya \
  --context "Choose the cheapest model that can reliably handle the request." \
  --label 'claude-haiku-4.5_(fast)' \
  --label 'claude-opus-5.5_(strong)' \
  --label 'claude-fable-5.1_(reasoning)' \
  --criterion 'claude-haiku-4.5_(fast)=Simple rewriting, extraction, or classification; no deep reasoning.' \
  --criterion 'claude-opus-5.5_(strong)=Complex coding, multi-file review, or tool use; accuracy matters.' \
  --criterion 'claude-fable-5.1_(reasoning)=Multi-step analysis, difficult debugging, or high uncertainty.'

claude-fable-5.1_(reasoning): 0.192
claude-haiku-4.5_(fast): 0.098
claude-opus-5.5_(strong): 0.710
predicted: claude-opus-5.5_(strong)
```

One local run using Ollaya: each score is the model's probability for that
label; `predicted:` is the top choice. This is Pixel's local Jev-style decision
mode. In the published typed-decision benchmark, Ollaya's `winnow:e4b` scored
0.722 accuracy; hosted TypeSafe Jev scored 0.738. [Benchmark details](https://pixel-cli.dev/benchmarks/#coding-decisions).

Clef-flash, Cloudflare's 9B decision model, is a typed-decision engine too:
`--remote-preset clef-ollama` runs it through Ollama (`ollama pull clef-flash`,
no key) and `clef-cloudflare` through Workers AI (API token and account id).
The 38.8 ms median latency and 98.76 case-exact on BFCL it reports against
Jev's 524.1 ms and 95.75 are Cloudflare's own figures, not a Pixel measurement.

Remote decisions (uses fast LLMs, not the System One model) can also return scores, through
OpenRouter, Ollama Cloud, DeepSeek, OpenCode Go, OpenAI, or a local
OpenAI-compatible endpoint (`--engine remote
--remote-preset openrouter|ollama|deepseek|opencode-go|openai|local`). On 14 public
coding prompts:

| Model | Provider (preset) | Score |
|---|---|---|
| deepseek-v4.1-flash | Ollama Cloud (`ollama`) | **14/14 (1.00)** |
| deepseek-v4-flash | Ollama Cloud (`ollama`) | 13/14 (0.93) |
| gpt-oss:120b / gpt-oss:20b | Ollama Cloud (`ollama`) | 13/14 (0.93) |
| nemotron-3-ultra | Ollama Cloud (`ollama`) | 12/14 (0.86) |
| google/gemini-3.1-flash-lite | OpenRouter (`openrouter`) | 11/14 (0.79) |
| qwen3.5:397b | Ollama Cloud (`ollama`) | 10/14 (0.71) |

These are benchmark accuracy scores, not per-decision probabilities.
[Full model results and method](docs/bench/decide-bakeoff.md).

## More

[Documentation](https://pixel-cli.dev/docs/) · [Manual setup](docs/manual-setup.md) · [Benchmarks](https://pixel-cli.dev/benchmarks/) · [GitHub Actions](docs/github-actions.md)

[Architecture](ARCHITECTURE.md) · [Contributing](CONTRIBUTING.md) · [Security](SECURITY.md) · [Report a bug](https://github.com/Pixel-CLI/pixel/issues/new?template=bug_report.yml)

MIT licensed. macOS (Apple Silicon) and Linux. See [NOTICE](NOTICE) for attribution.
