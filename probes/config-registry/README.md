# Configuration declaration identities

When multiple declarations share a key spelling, the registry keeps one
candidate per source coordinate instead of promoting the first declaration
to a unique default. Each candidate carries its own value and a bounded
source excerpt showing the surrounding scope. At most four candidates are
quoted; an explicit omission count preserves the uncertainty beyond that cap.

This is evidence of declarations, not lexical binding resolution. Ordinary
constant usages are visible only when present in the excerpt; they are not
claimed as resolved reads. Environment lookups remain source evidence.
Loader, mode, ambient environment and override precedence are unverified, so
multiple declarations stay candidates and no effective value is asserted.

The existing producer/consumer chain stays local to the probe:
`scan → registry → rank → emit → declaration_candidates/evidence_block`.
`rank` chooses key spellings; `emit` preserves the separate declarations of
each chosen key. The JSON request and numbered-source block contract do not
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

Full loader precedence and choosing a resolved binding are separate work in
#1233. This unit does not revive the rejected `config-sites` enclosing-body
prototype and makes no zero-tool-call claim.
