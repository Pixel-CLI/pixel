---
title: "Why Pixel uses Rust"
description: "Fast local retrieval, explicit resource ownership, and a native executable. Why we keep Rust in Pixel’s core, acknowledge its build costs, and leave room for other languages."
---

Pixel keeps Rust for its retrieval engine because the existing implementation brings together the properties the product needs: fast byte-oriented search, control over memory, native parsing and storage, and a CLI people can run locally. Replacing it needs to improve the time to a correct, validated change while preserving those properties.

That is an engineering decision we can revisit. Our exploratory language audit did not establish a better replacement, and it did not establish that Rust is the best language for every part of Pixel.

## What Rust does for Pixel

Pixel indexes source files, resolves symbols and calls, searches Git history, and retrieves context from agent transcripts. Its daemon handles queries while files change and background work updates the index. A short-lived CLI can also run the service in-process when the daemon is unavailable.

These responsibilities make three properties particularly useful:

- **Control over data and resources.** Search works on bytes, posting lists and persisted indexes. Rust lets this implementation control allocations and resource lifetimes without a tracing garbage collector. Ownership helps make sharing and cleanup explicit; it does not prove the search algorithm or freshness rules correct.
- **Native integration already in place.** Pixel’s Rust code integrates Tree-sitter grammars, SQLite and embedding backends. Replacing the language means preserving extraction, query and model behavior as well as connecting to the same libraries.
- **A native CLI distribution.** Users install a compiled executable rather than a language runtime for the core. The release workflow builds macOS ARM64 and Linux musl ARM64 and x86-64 binaries. Embedding features differ by target: Linux uses model2vec; the macOS build also includes fastembed.

Other languages can provide some or all of these properties. Rust’s advantage here is that they already work together in Pixel, with tests and compatibility contracts around them. The [architecture](https://github.com/Pixel-CLI/pixel/blob/main/ARCHITECTURE.md) and [release workflow](https://github.com/Pixel-CLI/pixel/blob/main/.github/workflows/release.yml) describe that implementation.

## Build time is a real cost

A language that makes each edit slow to validate can limit development, including development with coding agents. We take that concern seriously.

The useful measure is elapsed time from an edit to a correct, validated change. It includes implementation, compilation, linking, tests, linting, repair and CI. A quick compiler cannot compensate for missing checks; a fast executable cannot make developer waiting disappear.

Pixel already has a `dev-release` profile that disables thin LTO and increases codegen units compared with the shipping release profile. Contributors use focused checks during editing and the applicable full gates when a reviewable change is ready. These are existing workflow choices, not a newly demonstrated productivity gain from the audit. See [CONTRIBUTING.md](https://github.com/Pixel-CLI/pixel/blob/main/.github/CONTRIBUTING.md).

## What the exploratory audit established

The October 2026 [language audit task](https://github.com/Pixel-CLI/pixel/issues/526) considered Rust, TypeScript on Bun, Zig, Go, C++, C# with Native AOT, and Ruby compiled with Spinel.

Small native integration probes worked in all six alternatives on the available macOS ARM64 host. Access to a parser or database was therefore not, by itself, a reason to reject them. Matching regex and byte semantics, resource cleanup, embedding features and packaging still required separate work.

The audit also implemented a normalized search slice in Rust, Go, C++ and Bun: gram extraction, posting-list intersection and exact match verification. The tested alternatives remained slower than the Rust control after a bounded optimization pass. This is a result about those implementations, not a general language ranking. They did not implement Pixel’s complete production index, and library and data-structure differences were not fully isolated.

**This was a partial, local investigation, not a reproducible public benchmark release.** Its raw artifacts are not published with this page, so we do not present a timing chart or speedup claim here. Full product equivalence, native Linux execution, sustained concurrent operation and controlled agent productivity remain unmeasured. The audit did not show that migrating would make validated development faster.

Our [published benchmarks](/benchmarks/) answer different questions about Pixel’s behavior and comparisons with other tools. They should not be read as language benchmarks.

## Where another language could fit

Keeping the engine in Rust does not require every surrounding tool to use it.

| Boundary | Current choice or next step |
| --- | --- |
| Search, graph resolution, storage and daemon | Keep Rust and its existing correctness contracts. |
| Benchmark analysis and contributor tooling | Continue using Python where the repository already uses it. |
| Agent integrations | Use the host’s extension language where appropriate; Pixel already ships a TypeScript Pi extension. |
| Browser and model workflow orchestration | Consider a small TypeScript experiment calling the Rust core through structured requests. This is a proposal, not an implemented migration or measured gain. |

A useful experiment would let an agent change a workflow without rebuilding the engine, while still checking outputs, errors, cancellation and cleanup. It would also account for the added runtime or packaging requirement. Crossing a language boundary is worthwhile only when its benefit exceeds the maintenance cost.

We would keep frequent hooks, installation and guarded Git mutations in Rust for now. Their startup, configuration-preservation and recovery behavior deserve more evidence than “this would be easier to write elsewhere.”

## What would change our decision

We would reconsider a component when an alternative demonstrates faster edit-to-validated changes under equivalent tests, preserves its runtime and memory requirements, and works on the supported release targets. The comparison must include native dependencies, packaging, failure recovery and the cost of maintaining the boundary.

For now, the next useful investment is measuring and improving the Rust development loop. A narrowly scoped experiment at an orchestration boundary can happen alongside that work. A production rewrite needs stronger evidence than this audit provides.
