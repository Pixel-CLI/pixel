// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! `pixel-daemon` (`specs/ops.md`): one daemon per repository on a Unix
//! socket, one JSON [`Op`](pixel_proto::Op) per line in and one
//! [`Envelope`](pixel_proto::Envelope) per line out.
//!
//! [`Service::handle`] is the single entry point; the socket server
//! ([`serve`]) and the in-process fallback of the CLI both call it. The
//! thread that owns the `Service` is the only writer; reads are served from
//! replicas of the last published generation, and [`plane`] classifies every
//! op as a read or a write with no wildcard arm.

pub mod api;
mod brief;
pub mod edit;
pub mod meaning;
mod plane;
pub mod serve;
pub mod service;
pub mod wire;

pub use api::{Request, Response, ServeError, failure_response};
pub use serve::{pid_path, run, socket_path};
pub use service::Service;

/// Fixtures of the daemon's tests: a real git repository in a temp dir, the
/// text index of it, and its graph built by the real pipeline.
#[cfg(test)]
pub(crate) mod testutil {
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::time::{Duration, Instant};

    use pixel_graph::store::Store;
    use pixel_index::TrigramExtractor;
    use pixel_index::indexset::IndexSet;

    /// Runs `git args` in `dir` as a fixed author, without the machine's config.
    pub(crate) fn git(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .expect("git runs");
        assert!(out.status.success(), "git {args:?}: {out:?}");
    }

    /// Files added to every fixture repository so a word in a few of them is
    /// rare, not ubiquitous.
    pub(crate) const FILLER: usize = 40;

    /// A committed repository holding `files` and [`FILLER`] files about nothing.
    pub(crate) fn repo(files: &[(&str, &str)]) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonical temp dir");
        let filler: Vec<(String, &str)> = (0..FILLER)
            .map(|n| (format!("filler/f{n:02}.txt"), "nothing here\n"))
            .collect();
        let named = files.iter().map(|(path, body)| ((*path).to_owned(), *body));
        for (path, body) in named.chain(filler) {
            let file = root.join(path);
            std::fs::create_dir_all(file.parent().expect("a parent")).expect("dirs");
            std::fs::write(file, body).expect("write");
        }
        git(&root, &["init", "-q"]);
        git(&root, &["add", "."]);
        git(&root, &["commit", "-qm", "fixture"]);
        (dir, root)
    }

    /// The text index of `root` and, with `graph`, its code graph built and
    /// opened for reading.
    pub(crate) fn open(root: &Path) -> (IndexSet, Store) {
        pixel_graph::refresh(
            root,
            Some(Duration::from_secs(5)),
            Some(Instant::now() + Duration::from_secs(30)),
        )
        .expect("the fixture graph builds");
        (
            IndexSet::open_or_build(root, Box::new(TrigramExtractor)).expect("the index builds"),
            Store::open(root).expect("the graph opens"),
        )
    }
}
