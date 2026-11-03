// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Hand-labelled Ruby fixtures (C-10).
//!
//! Each test writes a few Ruby files, runs them through extraction and
//! `link` as an index run does, and states the exact set of resolved edges
//! as `(source, kind, line, target)`, each symbol by its name, labelled by
//! reading the fixture as Ruby runs it; the sites the rules cannot prove are
//! named with no target. One module per class of input: the classes
//! `design/testing.md` names for C-10 (aliasing, imports, shadowing, dynamic
//! dispatch), then ancestors and concerns, constant receivers, callbacks,
//! routes, associations and the project boundary, and the methods defined
//! other than by a plain `def` in a body (`definers`).
//!
//! The v1 tests these port are cited by name (`ruby-resolve.md` §18 and
//! `ruby-project-routes.md` §14 of the port specs); where the MVP's rules
//! give another answer, the comment names the decision that changed it.

mod ancestors;
mod audited;
mod calls;
mod constants;
mod definers;
mod gems;
mod projects;
mod rails;
mod references;
mod rspec;
mod rspec_shared;
mod scoped;
mod stubs;
mod typed;
mod unplaced;

use rustc_hash::FxHashMap as HashMap;
use std::collections::BTreeSet;
use std::convert::Infallible;
use std::path::PathBuf;

use mvp_model::hash::Hash;
use mvp_model::{Delta, EdgeKind, FileRow, FileRows, Kept, Symbol};

use crate::mvp_index::extract::{Extracted, rows};
use crate::mvp_index::lang;
use crate::mvp_index::link::link;
use crate::mvp_index::tree::{Tree, TreeFile};

/// A resolved edge or an undecided site: source name, kind, line, and the
/// target's name (resolved) or the target as written (unresolved).
type Row = (String, EdgeKind, u32, String);

/// Indexes `files` as a first run does: the files the tree scan keeps (by
/// name, then by content: a shell binstub is not Ruby), their rows, then
/// every site decided.
fn index(files: &[(&str, &str)]) -> Vec<FileRows> {
    let delta = run(files, &[]);
    delta.add
}

/// The facts extraction records for `code`, written as `lib/x.rb`.
pub(in crate::mvp_index::ruby) fn facts_of(code: &str) -> Vec<super::Fact> {
    let rows = index(&[("lib/x.rb", code)]);
    let file = rows.iter().find(|r| r.file.path == "lib/x.rb");
    super::Fact::decode_all(&file.expect("lib/x.rb").facts)
}

/// Indexes `files` as a run after `before` does: the files whose content
/// changed are extracted again, the others' rows are kept, and every site
/// is decided again. Returns the rows of the new generation.
fn update(before: &[FileRows], files: &[(&str, &str)]) -> Vec<FileRows> {
    let delta = run(files, before);
    let dropped = |path: &str| delta.drop.iter().any(|d| d == path);
    let redirected: HashMap<(&str, u32, &str), Option<String>> = delta
        .redirect
        .iter()
        .map(|(edge, dst)| {
            (
                (edge.src.as_str(), edge.line, edge.target.as_str()),
                dst.clone(),
            )
        })
        .collect();
    let mut after: Vec<FileRows> = before
        .iter()
        .filter(|rows| !dropped(&rows.file.path) && files.iter().any(|(p, _)| *p == rows.file.path))
        .cloned()
        .collect();
    for rows in &mut after {
        for edge in &mut rows.edges {
            let key = (edge.src.as_str(), edge.line, edge.target.as_str());
            if let Some(dst) = redirected.get(&key) {
                edge.dst = dst.clone();
            }
        }
    }
    after.extend(delta.add);
    after.sort_by(|a, b| a.file.path.cmp(&b.file.path));
    after
}

/// One run of `plan`, extraction and `link` over `files`, against the
/// generation `before` (empty for a first run).
fn run(files: &[(&str, &str)], before: &[FileRows]) -> Delta {
    let mut kept: Vec<(&str, &str)> = files
        .iter()
        .copied()
        .filter(|(path, content)| {
            lang::wanted(path).is_some_and(|l| lang::accepts(l, path, content.as_bytes()))
        })
        .collect();
    kept.sort();
    let tree = Tree {
        root: PathBuf::new(),
        commit: None,
        dirty: false,
        files: kept
            .iter()
            .map(|(path, content)| TreeFile {
                path: (*path).to_owned(),
                lang: lang::wanted(path).expect("kept"),
                hash: Hash::of(content.as_bytes()),
                bytes: content.len() as u64,
                stat: None,
            })
            .collect(),
        skipped: Vec::new(),
        untracked: Vec::new(),
        index: None,
        unsettled: false,
    };
    let indexed: Vec<FileRow> = before.iter().map(|rows| rows.file.clone()).collect();
    let extractor = (!before.is_empty()).then(lang::extractor);
    let plan = crate::mvp_index::link::plan(&tree, &indexed, extractor.as_deref(), &[]);
    let content: HashMap<&str, &str> = kept.iter().copied().collect();
    let extracted = Extracted {
        files: plan
            .extract
            .iter()
            .map(|file| {
                rows(
                    &file.path,
                    file.lang,
                    content[file.path.as_str()].as_bytes(),
                )
            })
            .collect(),
        stale: Vec::new(),
    };
    let kept_rows = |drop: &[String]| {
        let mut kept = Kept::default();
        for rows in before.iter().filter(|r| !drop.contains(&r.file.path)) {
            kept.files.push((rows.file.clone(), rows.facts.clone()));
            kept.symbols.extend(rows.symbols.iter().cloned());
            kept.edges.extend(rows.edges.iter().cloned());
        }
        Ok::<_, Infallible>(kept)
    };
    match link(&tree, &plan, extracted, kept_rows) {
        Ok(delta) => delta,
        Err(never) => match never {},
    }
}

fn symbol_names(rows: &[FileRows]) -> HashMap<&str, &Symbol> {
    rows.iter()
        .flat_map(|r| &r.symbols)
        .map(|s| (s.uid.as_str(), s))
        .collect()
}

/// Every resolved edge of `rows`, each end by its symbol's name (a file's
/// own symbol is named by its path).
fn resolved(rows: &[FileRows]) -> BTreeSet<Row> {
    let symbols = symbol_names(rows);
    rows.iter()
        .flat_map(|r| &r.edges)
        .filter_map(|e| {
            let dst = symbols[e.dst.as_deref()?];
            let src = symbols[e.src.as_str()];
            Some((src.name.clone(), e.kind, e.line, dst.name.clone()))
        })
        .collect()
}

/// Every site of `rows` left without a target, with its target as written.
fn unresolved(rows: &[FileRows]) -> BTreeSet<Row> {
    let symbols = symbol_names(rows);
    rows.iter()
        .flat_map(|r| &r.edges)
        .filter(|e| e.dst.is_none())
        .map(|e| {
            let src = symbols[e.src.as_str()];
            (src.name.clone(), e.kind, e.line, e.target.clone())
        })
        .collect()
}

/// The rows a test labels by hand.
fn labelled(rows: &[(&str, EdgeKind, u32, &str)]) -> BTreeSet<Row> {
    rows.iter()
        .map(|(src, kind, line, dst)| ((*src).to_owned(), *kind, *line, (*dst).to_owned()))
        .collect()
}

/// Asserts that each named site is among the unresolved ones.
#[track_caller]
fn assert_unresolved(rows: &[FileRows], sites: &[(&str, EdgeKind, u32, &str)]) {
    let unresolved = unresolved(rows);
    let missing: Vec<Row> = labelled(sites).difference(&unresolved).cloned().collect();
    assert!(missing.is_empty(), "resolved or absent: {missing:#?}");
}
