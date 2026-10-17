# mvp-lexicon origin and ownership

Originally copied from `Pixel-CLI/pixel-mvp` at `35aaf1ca435c3de8bdc4b2a3f74399a0aaeeb29c`
(`35aaf1c`). The original code is MIT licensed.

Pixel owns and maintains this component directly. Fixes, tests and reviews
belong in `Pixel-CLI/pixel`; no change or synchronization in pixel-mvp is
required. The `mvp-` name records its origin, not an external dependency.

See [the maintenance handover](../../docs/mvp-handover.md) for the component
inventory and remaining product work.

## Maintaining the lexicon

Edit the compact lemma data in `generate.py`, then run from the repository:

```sh
python3 -I crates/mvp-lexicon/generate.py crates/mvp-lexicon/fr-en.tsv
python3 -I crates/mvp-lexicon/test_generate.py
```

The generator is copied from pixel-mvp `e91e8da` with only license headers
added. CI reproduces the shipped TSV in an isolated directory. A vocabulary
change also needs a brief-gate comparison on the French prompts; regeneration
alone proves reproducibility, not retrieval quality.
