# mvp-lexicon (vendored)

`fr-en.tsv` is a copy of `crates/pixel-adapter/lexicon/fr-en.tsv` of
pixel-mvp at commit `4b23d4cdf72a283f5d60bc474c1d7bae1b6689bb` (`4b23d4c`), made by
`scripts/dev/vendor-mvp.sh`, byte for byte. It is data, not a crate: one
French form per line, a tab, and the English words code names its notion
with, or `-` for a word that carries no meaning in a request. ADR-0032 of
the pixel-mvp research repository decided the lexicon; ADR-0036 moves it
into Pixel's prompt-submit brief.

- **Do not edit `fr-en.tsv` here.** pixel-mvp rebuilds it with
  `crates/pixel-adapter/lexicon/generate.py` and measures a change on its
  French development set before it ships: a change lands there first, then
  comes here through the next sync, which names its commit.
- **Sync:** `scripts/dev/vendor-mvp.sh <pixel-mvp checkout> <commit> lexicon`,
  then the gates, in a pull request that names the commit.
