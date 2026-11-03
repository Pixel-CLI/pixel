---
title: "Pixel vs WarpGrep"
description: "Pixel and WarpGrep measured on the same plain-English queries: Pixel puts the right file first more often and answers faster, locally; WarpGrep hands back the code spans themselves and claims its gains at the agent level."
tool: "warpgrep"
---

## How they differ

WarpGrep is Morph's search subagent: a model trained with reinforcement learning (`morph-warp-grep-v2.1`) drives ripgrep, directory listings and file reads on your tree for up to six turns, then hands the agent the code spans it judged relevant. It keeps no index, so there is nothing to build, and each search is a paid call to Morph's API. `pixel search-meaning` answers from an index in `.pixel/`: a ranked list of files with the matching chunk of each, computed on the machine.

## What leaves the machine

WarpGrep's SDK runs its tools locally, but their results, grep lines and reads of up to 800 lines, are the model's next input and are sent to Morph's API. Pixel's search sends nothing: the embedding model is downloaded once, from Hugging Face, on first use, and the index stays on your machine.

## Reading the numbers

WarpGrep's figures come from its one run, on 2026-09-27 beside Pixel 0.5.2, on the same queries and code; it was not re-run for Pixel 0.6.0, since each search is a paid call. It names about one file per answer, so its top-10 score is close to its first-answer score, and it is not deterministic: of 19 cases scored in two runs on the same day, 2 changed verdict. Its own claim is agent-level, on SWE-Bench Pro: a coding agent with WarpGrep solves more tasks with fewer input tokens. That is not what this benchmark measures, for either tool.
