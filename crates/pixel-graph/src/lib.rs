// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! pixel-graph — symbols, imports, tiered call resolution, analyses.
//!
//! Module ownership (parallel development contract — do not fold modules
//! into each other):
//! - `store`: SQLite persistence + shared row types (THE schema contract)
//! - `extract`: tree-sitter per-file symbol/import/call-site extraction
//! - `imports`: import-spec → file resolution
//! - `resolve`: tiered edge resolution + epistemic envelope
//! - `build`: whole-repo build/update orchestration
//! - `impact`, `trace`, `process`, `cluster`, `changes`: analyses
//! - `predicate`: bounded reachability with witnesses (`pixel evaluate`)
//! - `mvp_index`: pixel-mvp's indexer for Ruby and Rust (imported; see its
//!   `ORIGIN.md`), returning rows and writing nothing
//! - `mvp_adapter`: pixel-mvp's Rust index in this graph's rows (`mvp_index`
//!   and the vendored `mvp-*` crates), and `mvp_adapter::ruby`, its Ruby answer merged site
//!   by site with this graph's; `shadow`: this graph's Rust edges against it

pub mod build;
pub mod changes;
pub mod cluster;
pub mod concept;
pub mod concept_resolve;
pub mod cycles;
pub mod extract;
pub mod impact;
pub mod imports;
pub mod mvp_adapter;
pub mod mvp_index;
pub mod plan;
pub mod predicate;
pub mod process;
pub mod rename;
pub mod resolve;
pub mod review;
pub mod shadow;
pub mod store;
pub mod targets;
pub mod trace;

pub use impact::split_ident_words;
pub use store::{
    EdgeKind, EdgeRow, Envelope, FileRow, GraphStore, StoreError, SymbolKind, SymbolRow, Tier,
};

#[cfg(test)]
pub(crate) mod testutil {
    //! The crate's git fixture helpers: real `git` in a temporary directory,
    //! with a fixed identity and no global configuration.

    use std::path::Path;

    /// Runs `git -C root args…`, and fails the test when git does.
    pub(crate) fn git(root: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
    }

    /// A repository at `root` whose one commit holds `files`, each written with
    /// its content.
    pub(crate) fn init_repo(root: &Path, files: &[(&str, &str)]) {
        git(root, &["init", "-q"]);
        for (rel, content) in files {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
        }
        git(root, &["add", "."]);
        git(root, &["commit", "-qm", "fixture"]);
    }
}
