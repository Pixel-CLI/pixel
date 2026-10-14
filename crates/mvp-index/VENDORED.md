# mvp-index (vendored)

This crate is a copy of `crates/pixel-index` of pixel-mvp, the
experimental successor of Pixel's index, at commit `d5c7cf7fd006ade26a4de78c0bb9ba720cc6ca4e`
(`d5c7cf7`), made by `scripts/vendor-mvp.sh`. ADR-0035 of the pixel-mvp
research repository decided the copy: the MVP's Rust and Ruby index
becomes the source of Pixel's code graph for the languages where it is
shown to be right, behind a switch.

- **Do not edit `src/` here.** A fix lands in pixel-mvp first, then comes
  here through the next sync, which names its commit. The only
  differences from the source are mechanical: the crate names
  (`pixel-model`, `pixel-store`, `pixel-index` read `mvp-model`,
  `mvp-store`, `mvp-index`), the SPDX header of each `.rs` file,
  and `cargo fmt` over the result.
- **`Cargo.toml` is this workspace's**: the dependency versions follow
  the workspace's `Cargo.lock`, and the crate keeps pixel-mvp's own lint
  table instead of the workspace's, so that a sync stays a copy. The
  nightly mutation campaign leaves the crate out (`.cargo/mutants.toml`):
  a survivor could only be fixed in pixel-mvp.
- **Sync:** `scripts/vendor-mvp.sh <pixel-mvp checkout> <commit> index`,
  then the gates, in a pull request that names the commit.
