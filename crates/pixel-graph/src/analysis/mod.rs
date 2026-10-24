// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! Questions asked of a published generation (`specs/graph.md`, "Analyses"):
//! `impact` (the blast radius of a symbol), `changes` (which symbols a diff
//! touches), `trace` (the call path between two symbols) and `review` (what
//! a reviewer would flag first); `context` fits one symbol's source to a token
//! budget. Each reads a [`Store`](crate::store::Store)
//! and returns a plain value; none writes, parses or runs git.
//!
//! Every answer is a lower bound: a site no rule could resolve is counted
//! (`open_sites`), never listed as a caller, and a caller found by a guess is
//! labelled `Likely`.

pub mod changes;
pub mod concept;
pub mod context;
pub mod impact;
pub mod resolve;
pub mod review;
pub mod trace;

#[cfg(test)]
pub(crate) mod fixture {
    //! A small crate with calls across files, indexed into a store.

    use crate::refresh;
    use crate::store::Store;
    use crate::testutil::repo_with;

    pub(crate) const MANIFEST: &str = "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n";

    /// `b::go` calls `a::run` and `c::other`; `tests/it.rs` calls `b::go`;
    /// `d::lonely` calls nothing and nothing calls it.
    pub(crate) fn project() -> (tempfile::TempDir, Store) {
        let repo = repo_with(&[
            ("Cargo.toml", MANIFEST),
            (
                "src/lib.rs",
                "pub mod a;\npub mod b;\npub mod c;\npub mod d;\n",
            ),
            ("src/a.rs", "pub fn run(x: u32) -> u32 {\n    x\n}\n"),
            ("src/c.rs", "pub fn other() -> u32 {\n    7\n}\n"),
            (
                "src/b.rs",
                "use crate::a::run;\nuse crate::c::other;\n\npub fn go() -> u32 {\n    run(1) + other()\n}\n",
            ),
            ("src/d.rs", "pub fn lonely() {}\n"),
            (
                "tests/it.rs",
                "use demo::b::go;\n\nfn t() {\n    go();\n}\n",
            ),
        ]);
        refresh(repo.path(), None, None).expect("refresh");
        let store = Store::open(repo.path()).expect("store");
        (repo, store)
    }
}
