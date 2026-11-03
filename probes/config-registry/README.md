# Configuration declaration identities

When multiple declarations share a key spelling, the registry keeps one
candidate per source coordinate instead of promoting the first declaration
to a unique default. Each candidate carries its own value and a bounded
source excerpt showing the surrounding scope. At most four candidates are
quoted; an explicit omission count preserves the uncertainty beyond that cap.

This is evidence of declarations, not lexical binding resolution. Ordinary
constant usages are visible only when present in the excerpt; they are not
claimed as resolved reads. Environment lookups remain source evidence.
Ambient environment and active deployment mode are unverified, so multiple
declarations stay candidates and no effective value is asserted.

For questions about environment files, a bounded Python AST lane also quotes
dynamic environment-name lookups whose result is forwarded to a same-class
helper, following at most three methods through to a mapping write. The
constructor, loader conditions and mode parameters travel with that chain.
This exposes the conditional overwrite mechanism rather than inventing a
value from a default. Class and source coordinates identify each candidate;
same-class calls are not resolved runtime dispatch. Deployment mode and actual
invocation order stay unknown. In particular, a loader's `silent` failure mode
does not establish development/production selection or `.env` file priority.

The lane reads only tracked regular Python files (first 256 sorted paths,
400 KiB per file), skipping tests, fixtures, vendor files and symlinks. It
recognizes direct `import os`, a named-parameter `os.environ.get`/`os.getenv`
lookup assigned to a local and forwarded as the first helper argument. It
does not execute repository code or read the file named by the environment.
It emits at most two loader candidates, 80 source rows each, within 6000
characters; omissions are explicit. Nested/decorated helpers and imports
between files are unsupported. Literal strings/numbers, f-string text and
comments are masked or omitted, while symbolic mapping writes survive.
Docstrings are omitted without crediting their source ranges.

The existing producer/consumer chain stays local to the probe:
`scan → registry → rank → emit → declaration_candidates/evidence_block`.
`rank` chooses key spellings; `emit` preserves the separate declarations of
each chosen key. `emit → loader_evidence → loader_source_rows` adds the
independent, intent-gated loader lane before that registry; a missing literal
key does not suppress a valid dynamic-name loader. The JSON request and numbered-source block contract do not
change. The existing bundled entry embeds this program on the next build;
no Rust or scorer change is needed.

Environment files are excluded from the general ripgrep scan. Only tracked,
regular environment templates and the supported mode files are read through
the separate environment lane; symlinks are omitted. Values of secret-looking
keys are masked in summaries, fallbacks and excerpts. Context containing a
secret-looking identifier is masked conservatively too. Both scan lanes share
the same file-size and line-size limits; line size counts UTF-8 bytes like rg.

Run the regression suite with
`python3 -I probes/config-registry/tests/test_probe.py`. Set `PROBE_UNDER_TEST`
to the old executable to reproduce the missing declaration and safety failures.

Full runtime precedence and choosing a resolved binding remain work in #1233.
This follows explicit AST argument forwarding and same-class helper candidates,
not the rejected `config-sites` token-matched enclosing-body rule. It makes no
zero-tool-call claim. See `docs/bench/config-precedence-1233/` for the frozen
comparison and the discarded dotenv-only hypothesis.
