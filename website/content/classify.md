---
title: "Optional classification"
description: "A model-backed decision aid, separate from Pixel’s local code index."
---

## When to use it

`pixel classify` chooses between labels you supply: task routing, severity or a review gate. It does not retrieve code and is not needed to try the local index. Unlike index queries, it uses a model and its answer is not deterministic.

Use `--criterion` to define each label and `--context` to explain the decision. The output gives a probability per label and `predicted:` names the highest score. Remote engines send the question to the configured provider; Ollaya runs a local model, and Clef-flash, Cloudflare's 9B decision model, runs through local Ollama (`clef-ollama`, no key) or Cloudflare Workers AI (`clef-cloudflare`, API token and account id). Model scores are a decision aid, not evidence that the choice is correct.

## Example and published evidence

The example below uses illustrative routing labels. Its scores are from one local run; they are not a comparison of the named coding models. Accuracy and latency figures are upstream measurements with different samples, not a Pixel reproduction.

{{< classify-example >}}

## Further reading

[Source, sample and scoring](/benchmarks/#coding-decisions) · [Jev comparison](/vs/jev/) · [Agent protocol](https://github.com/Pixel-CLI/pixel/blob/main/PIXEL.md)
